//! # fastsearch-pg
//!
//! Postgres 真源接入：幂等 schema/DDL、Chunk↔行映射、doc_id 级替换写路径、读取。
//! 仅依赖 pgvector + 逻辑复制，**不要求任何 `shared_preload_libraries` 原生扩展**
//! （托管 PG 可移植，见需求 N1b）。详见 [spec](../../docs/specs/12-pg.md)。

mod error;
mod sql;

pub use error::{PgError, Result};
pub use sql::{
    ann_index_sql, pgvector_search_sql, ChunkRow, SqlParam, VectorType, COLUMNS, PUBLICATION,
};

use fastsearch_core::Chunk;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, Row};

/// 连接配置。
#[derive(Debug, Clone)]
pub struct PgConfig {
    pub url: String,
    pub table: String,
    pub vector_dim: usize,
    pub vector_type: VectorType,
}

impl PgConfig {
    pub fn new(url: impl Into<String>) -> Self {
        PgConfig {
            url: url.into(),
            table: "fastsearch_chunks".to_string(),
            vector_dim: 384,
            vector_type: VectorType::HalfVec,
        }
    }
}

/// Postgres 真源句柄。
pub struct PgStore {
    client: Client,
    cfg: PgConfig,
}

impl PgStore {
    /// 连接（后台驱动连接 future）。
    pub async fn connect(cfg: PgConfig) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(&cfg.url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("fastsearch-pg connection error: {e}");
            }
        });
        Ok(PgStore { client, cfg })
    }

    /// 幂等建表/扩展/索引/publication。
    pub async fn ensure_schema(&self) -> Result<()> {
        for stmt in sql::ddl(&self.cfg.table, self.cfg.vector_type, self.cfg.vector_dim) {
            self.client.batch_execute(&stmt).await?;
        }
        Ok(())
    }

    /// doc_id 级替换：事务内先删后批量插，保证原子（CDC 看到 delete+insert）。
    pub async fn upsert_doc(
        &mut self,
        collection: &str,
        doc_id: &str,
        chunks: &[Chunk],
    ) -> Result<u64> {
        let del = sql::delete_doc_sql(&self.cfg.table);
        let ins = sql::insert_sql(&self.cfg.table);
        let tx = self.client.transaction().await?;
        tx.execute(&del, &[&collection, &doc_id]).await?;
        let mut n = 0u64;
        for c in chunks {
            let row = ChunkRow::from_chunk(collection, c)?;
            let params: [&(dyn ToSql + Sync); 14] = [
                &row.collection,
                &row.doc_id,
                &row.chunk_id,
                &row.kind,
                &row.text,
                &row.page,
                &row.bbox,
                &row.heading_path,
                &row.section_id,
                &row.char_len,
                &row.modality,
                &row.media,
                &row.tenant,
                &row.acl,
            ];
            n += tx.execute(&ins, &params).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// 删除某 doc 全部 chunk。
    pub async fn delete_doc(&self, collection: &str, doc_id: &str) -> Result<u64> {
        let del = sql::delete_doc_sql(&self.cfg.table);
        Ok(self.client.execute(&del, &[&collection, &doc_id]).await?)
    }

    /// 读取某 doc 全部 chunk（按 chunk_id 升序）。
    pub async fn fetch_doc(&self, collection: &str, doc_id: &str) -> Result<Vec<Chunk>> {
        let q = sql::fetch_doc_sql(&self.cfg.table);
        let rows = self.client.query(&q, &[&collection, &doc_id]).await?;
        rows.iter().map(row_to_chunk).collect()
    }

    /// 全表读取 `(collection, Chunk)`（初始快照 bootstrap 用）。v1 全量；超大表分页为后续。
    pub async fn fetch_all_chunks(&self) -> Result<Vec<(String, Chunk)>> {
        let q = sql::fetch_all_sql(&self.cfg.table);
        let rows = self.client.query(&q, &[]).await?;
        rows.iter()
            .map(|r| Ok((r.try_get::<_, String>("collection")?, row_to_chunk(r)?)))
            .collect()
    }

    /// **写穿**（B6 §2）：把某 chunk 的向量写回 PG `embedding` 列（直查档的向量由此进 PG）。
    /// 向量以 `$1::text::vector` 文本传（免 pgvector ToSql 依赖）。返回更新行数。
    pub async fn set_embedding(
        &self,
        collection: &str,
        doc_id: &str,
        chunk_id: u64,
        embedding: &[f32],
    ) -> Result<u64> {
        let sql = format!(
            "UPDATE {} SET embedding = $1::text::vector \
             WHERE collection = $2 AND doc_id = $3 AND chunk_id = $4",
            self.cfg.table
        );
        let v = format_vector(embedding);
        Ok(self
            .client
            .execute(&sql, &[&v, &collection, &doc_id, &(chunk_id as i64)])
            .await?)
    }

    /// **B6 直查档**：pgvector ANN 检索（ANN 在 PG 跑）。`acl`+可翻译 `filter` 下推 SQL（精确）、
    /// 不可翻译子句走 SUPERSET + **Rust 精确后过滤**（守不变量 #5）；`iterative_scan` 让 HNSW
    /// 在选择性过滤下仍 filter-aware（pgvector ≥0.8）。over-fetch `k×over_fetch` 抵消后过滤损耗。
    /// 返回按余弦相似降序、同分按 GlobalId 升序（确定 tie-break）的 top-k `(citation_id, score)`。
    pub async fn vector_search(
        &self,
        query: &[f32],
        k: usize,
        over_fetch: usize,
        acl: Option<&fastsearch_core::AclFilter>,
        filter: Option<&fastsearch_core::Filter>,
    ) -> Result<Vec<(fastsearch_core::Scored, fastsearch_core::Citation)>> {
        if k == 0 {
            return Ok(vec![]);
        }
        let limit = k.saturating_mul(over_fetch.max(1)).max(k);
        let (sql, sparams) = pgvector_search_sql(&self.cfg.table, limit, acl, filter);
        // filter-aware：iterative scan + 提高 ef_search（会话级，对本直查连接生效）。
        self.client
            .batch_execute("SET hnsw.iterative_scan = relaxed_order; SET hnsw.ef_search = 100;")
            .await
            .ok(); // 旧版 pgvector 无此 GUC 时忽略（退化为普通后过滤）。
                   // $1 = 查询向量（以文本字面 '[..]' 传，SQL 内 ::vector 转换，免 pgvector ToSql 依赖）。
        let qvec = format_vector(query);
        let mut owned: Vec<Box<dyn ToSql + Sync>> = vec![Box::new(qvec)];
        for p in &sparams {
            owned.push(match p {
                SqlParam::Text(s) => Box::new(s.clone()),
                SqlParam::Int(i) => Box::new(*i),
                SqlParam::TextArray(a) => Box::new(a.clone()),
            });
        }
        let params: Vec<&(dyn ToSql + Sync)> =
            owned.iter().map(|b| &**b as &(dyn ToSql + Sync)).collect();
        let rows = self.client.query(&sql, &params).await?;

        // Rust 精确后过滤（filter + ACL 复核）+ 组装 Scored + Citation（含 page/bbox，供溯源）。
        let mut hits: Vec<(fastsearch_core::Scored, fastsearch_core::Citation)> =
            Vec::with_capacity(rows.len());
        for r in &rows {
            let row = PgVecRow::from_row(r)?;
            if let Some(f) = filter {
                if !f.eval(&row) {
                    continue;
                }
            }
            if let Some(a) = acl {
                if !a.visible(&row) {
                    continue;
                }
            }
            let scored = fastsearch_core::Scored {
                id: row.gid(),
                score: r.try_get::<_, f64>("score")?,
            };
            hits.push((scored, row.citation()));
        }
        hits.sort_by(|a, b| {
            b.0.score
                .partial_cmp(&a.0.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        hits.truncate(k);
        Ok(hits)
    }
}

/// f32 向量 → pgvector 文本字面 `[v1,v2,...]`（配合 SQL 内 `$1::text::vector`：先 text 再 vector，
/// 避免 tokio-postgres 把 `$1` 推断成 vector 类型而拒收 String，同 jsonb 写入的处理）。
fn format_vector(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// 直查返回行的字段视图，供 `Filter::eval`/`AclFilter::visible` 精确后过滤（实现 `FieldSource`）。
struct PgVecRow {
    collection: String,
    doc_id: String,
    chunk_id: i64,
    kind: String,
    modality: String,
    page: i32,
    section_id: i64,
    tenant: Option<String>,
    acl: Vec<String>,
    heading_path: Vec<String>,
    bbox: fastsearch_core::BBox,
    /// 解析出的媒资引用（供 Citation.media；time 也由它给）。
    media: Option<fastsearch_core::MediaRef>,
}

impl PgVecRow {
    fn from_row(r: &Row) -> Result<Self> {
        let media_json: Option<String> = r.try_get("media")?;
        let media =
            media_json.and_then(|j| serde_json::from_str::<fastsearch_core::MediaRef>(&j).ok());
        let bbox_json: String = r.try_get("bbox")?;
        let bbox = serde_json::from_str(&bbox_json)?;
        Ok(PgVecRow {
            collection: r.try_get("collection")?,
            doc_id: r.try_get("doc_id")?,
            chunk_id: r.try_get("chunk_id")?,
            kind: r.try_get("kind")?,
            modality: r.try_get("modality")?,
            page: r.try_get("page")?,
            section_id: r.try_get("section_id")?,
            tenant: r.try_get("tenant")?,
            acl: r.try_get("acl")?,
            heading_path: r.try_get("heading_path")?,
            bbox,
            media,
        })
    }

    fn time(&self) -> Option<fastsearch_core::TimeSpan> {
        self.media.as_ref().and_then(|m| m.time)
    }

    fn gid(&self) -> fastsearch_core::GlobalId {
        fastsearch_core::GlobalId {
            collection: self.collection.clone(),
            doc_id: self.doc_id.clone(),
            chunk_id: self.chunk_id as u64,
        }
    }

    /// 组装溯源引用（page/bbox/heading_path + media/time）。
    fn citation(&self) -> fastsearch_core::Citation {
        fastsearch_core::Citation {
            collection: self.collection.clone(),
            doc_id: self.doc_id.clone(),
            chunk_id: self.chunk_id as u64,
            page: self.page as u32,
            bbox: self.bbox,
            heading_path: self.heading_path.clone(),
            section_id: self.section_id as u64,
            time: self.time(),
            media: self.media.clone(),
        }
    }
}

impl fastsearch_core::FieldSource for PgVecRow {
    fn get(&self, field: &str) -> Option<fastsearch_core::FieldValue> {
        use fastsearch_core::FieldValue;
        match field {
            "kind" => Some(FieldValue::Str(self.kind.clone())),
            "modality" => Some(FieldValue::Str(self.modality.clone())),
            "doc_id" => Some(FieldValue::Str(self.doc_id.clone())),
            "collection" => Some(FieldValue::Str(self.collection.clone())),
            "tenant" => self.tenant.clone().map(FieldValue::Str),
            "page" => Some(FieldValue::Int(self.page as i64)),
            "section_id" => Some(FieldValue::Int(self.section_id)),
            "time_start_ms" => self.time().map(|t| FieldValue::Int(t.start_ms as i64)),
            "time_end_ms" => self.time().map(|t| FieldValue::Int(t.end_ms as i64)),
            _ => None,
        }
    }
    fn heading_path(&self) -> &[String] {
        &self.heading_path
    }
    fn acl(&self) -> &[String] {
        &self.acl
    }
}

/// tokio_postgres::Row → Chunk（经 ChunkRow）。
fn row_to_chunk(r: &Row) -> Result<Chunk> {
    let row = ChunkRow {
        collection: r.try_get("collection")?,
        doc_id: r.try_get("doc_id")?,
        chunk_id: r.try_get("chunk_id")?,
        kind: r.try_get("kind")?,
        text: r.try_get("text")?,
        page: r.try_get("page")?,
        bbox: r.try_get("bbox")?,
        heading_path: r.try_get("heading_path")?,
        section_id: r.try_get("section_id")?,
        char_len: r.try_get("char_len")?,
        modality: r.try_get("modality")?,
        media: r.try_get("media")?,
        tenant: r.try_get("tenant")?,
        acl: r.try_get("acl")?,
    };
    row.to_chunk()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, ChunkKind};

    fn sample(doc: &str, id: u64) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: format!("chunk {id} 内容"),
            page: id as u32,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec!["章".into()],
            section_id: 1,
            char_len: 5,
            media: None,
            tenant: None,
            acl: vec!["public".into()],
        }
    }

    /// 集成测试：仅当 `DATABASE_URL` 设置时运行；否则跳过（不算失败）。
    #[tokio::test]
    async fn integration_roundtrip() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_roundtrip: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_chunks_it".into();
        let mut store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");

        // 替换写入 3 个
        let chunks = vec![sample("a.pdf", 1), sample("a.pdf", 2), sample("a.pdf", 3)];
        let n = store
            .upsert_doc("kb", "a.pdf", &chunks)
            .await
            .expect("upsert");
        assert_eq!(n, 3);
        let got = store.fetch_doc("kb", "a.pdf").await.expect("fetch");
        assert_eq!(got.len(), 3);
        assert_eq!(got, chunks);

        // 替换为 2 个（旧的被删）
        let chunks2 = vec![sample("a.pdf", 10), sample("a.pdf", 11)];
        store
            .upsert_doc("kb", "a.pdf", &chunks2)
            .await
            .expect("upsert2");
        let got2 = store.fetch_doc("kb", "a.pdf").await.expect("fetch2");
        assert_eq!(got2.len(), 2);

        // 删除后为空
        store.delete_doc("kb", "a.pdf").await.expect("delete");
        assert_eq!(
            store.fetch_doc("kb", "a.pdf").await.expect("fetch3").len(),
            0
        );
    }

    /// B6 直查集成：pgvector ANN + ACL 下推 + filter-aware（需 DATABASE_URL）。
    #[tokio::test]
    async fn integration_pgvector_search() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_pgvector_search: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_vec_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector; // 精确 vector(4)，便于断言
        let mut store = PgStore::connect(cfg).await.expect("connect");
        // 干净重建表（schema 可能变）。
        store
            .client
            .batch_execute("DROP TABLE IF EXISTS fastsearch_vec_it")
            .await
            .ok();
        store.ensure_schema().await.expect("schema");
        store
            .client
            .batch_execute(&sql::ann_index_sql("fastsearch_vec_it"))
            .await
            .ok();

        // 4 个 chunk：1=image/team-a, 2=paragraph/team-a, 3=paragraph/team-b, 4=paragraph/team-b。
        let mut chunks = vec![
            sample("d.pdf", 1),
            sample("d.pdf", 2),
            sample("d.pdf", 3),
            sample("d.pdf", 4),
        ];
        chunks[0].kind = ChunkKind::Image;
        for (i, c) in chunks.iter_mut().enumerate() {
            c.tenant = Some("acme".into());
            c.acl = vec![if i < 2 { "team-a" } else { "team-b" }.into()];
        }
        store
            .upsert_doc("kb", "d.pdf", &chunks)
            .await
            .expect("upsert");

        // 写 embedding（正交单位向量）：1=[1,0,0,0] ... 4=[0,0,0,1]。
        for id in 1..=4u64 {
            let mut e = vec![0.0f32; 4];
            e[(id - 1) as usize] = 1.0;
            store
                .client
                .execute(
                    "UPDATE fastsearch_vec_it SET embedding = $1::text::vector WHERE chunk_id = $2",
                    &[&format_vector(&e), &(id as i64)],
                )
                .await
                .expect("set embedding");
        }

        let acl_a = fastsearch_core::AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        // 查最接近 [1,0,0,0]（=chunk 1）+ team-a ACL → 命中 team-a（chunk 1/2），不见 team-b。
        let q = vec![0.9f32, 0.1, 0.0, 0.0];
        let hits = store
            .vector_search(&q, 5, 4, Some(&acl_a), None)
            .await
            .expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0.id.chunk_id, 1, "最近邻应为 chunk 1");
        // citation 含真实 page（来自 PG），不是退化的 0。
        assert_eq!(hits[0].1.page, 1);
        for h in &hits {
            assert!(h.0.id.chunk_id <= 2, "越权：team-b（chunk 3/4）不应可见");
        }

        // filter-aware：查最接近 [0,0,0,1]（=chunk 4，team-b）但过滤 modality=image →
        // 只有 chunk 1 是 image。iterative scan + 下推 WHERE 仍能召回到它（不因后过滤崩）。
        let only_image = fastsearch_core::Filter::Eq(
            "modality".into(),
            fastsearch_core::FieldValue::Str("image".into()),
        );
        let q2 = vec![0.0f32, 0.0, 0.0, 1.0];
        let hits2 = store
            .vector_search(&q2, 3, 8, None, Some(&only_image))
            .await
            .expect("search2");
        assert_eq!(hits2.len(), 1, "仅 1 个 image");
        assert_eq!(hits2[0].0.id.chunk_id, 1);

        store
            .client
            .batch_execute("DROP TABLE IF EXISTS fastsearch_vec_it")
            .await
            .ok();
    }
}
