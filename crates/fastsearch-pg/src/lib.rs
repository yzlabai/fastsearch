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

/// `ensure_schema` 的事务级 advisory lock key（固定常量，全副本同值才能互斥）。任意 i64；
/// 取自 ASCII `"fss_ddl\0"` 的高位字节，避免与运维自用 advisory key 偶然撞号。
const SCHEMA_DDL_LOCK_KEY: i64 = 0x6673_735f_6464_6c00;

/// Postgres 真源句柄。
pub struct PgStore {
    client: Client,
    cfg: PgConfig,
}

impl PgStore {
    /// 连接（后台驱动连接 future）。表名经标识符校验后才用于 SQL 拼接（防御性：表名是运维配置、
    /// 非客户端输入，但若未来被外部影响，此校验阻断注入面）。
    pub async fn connect(cfg: PgConfig) -> Result<Self> {
        validate_identifier(&cfg.table)?;
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
        // **并发 boot 安全**：引擎是多副本/无状态（CLAUDE.md），多副本同时 boot 都会跑这段
        // 幂等 DDL。`CREATE EXTENSION/TABLE/INDEX/PUBLICATION ... IF NOT EXISTS` 在 Postgres
        // 里**仍有 TOCTOU 竞态窗口**（并发执行可报 "tuple concurrently updated" / 重复键 /
        // "relation already exists"）。用**事务级 advisory lock**把整段 DDL 串行化：同 key 的
        // 并发调用排队，锁随 COMMIT/ROLLBACK 自动释放（异常安全，无需手动解锁）。DDL 幂等，
        // 后到的副本拿锁后看到 schema 已建好、各 IF NOT EXISTS 空转。
        let mut batch = String::from("BEGIN;\n");
        batch.push_str(&format!(
            "SELECT pg_advisory_xact_lock({SCHEMA_DDL_LOCK_KEY});\n"
        ));
        for stmt in sql::ddl(&self.cfg.table, self.cfg.vector_type, self.cfg.vector_dim) {
            batch.push_str(&stmt);
            batch.push('\n');
        }
        batch.push_str("COMMIT;\n");
        self.client.batch_execute(&batch).await?;
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
            let params: [&(dyn ToSql + Sync); 18] = [
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
                &row.media_bytes,
                &row.image_vector_status,
                &row.time_start_ms,
                &row.time_end_ms,
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

    /// 按主键取 inline 媒资字节（媒资网关 `/v1/asset` Inline 路径，MM6-inline 用）。
    /// 无该行 / `media_bytes` 为 NULL → `Ok(None)`。字节是 PG 真源、引擎派生层不持 → 按需直查。
    pub async fn fetch_media_bytes(
        &self,
        collection: &str,
        doc_id: &str,
        chunk_id: u64,
    ) -> Result<Option<Vec<u8>>> {
        let q = sql::fetch_media_bytes_sql(&self.cfg.table);
        let rows = self
            .client
            .query(&q, &[&collection, &doc_id, &(chunk_id as i64)])
            .await?;
        match rows.first() {
            Some(r) => Ok(r.try_get::<_, Option<Vec<u8>>>("media_bytes")?),
            None => Ok(None),
        }
    }

    /// 全表读取 `(collection, Chunk)`（初始快照 bootstrap 用）。v1 全量；超大表分页为后续。
    pub async fn fetch_all_chunks(&self) -> Result<Vec<(String, Chunk)>> {
        let q = sql::fetch_all_sql(&self.cfg.table);
        let rows = self.client.query(&q, &[]).await?;
        rows.iter()
            .map(|r| Ok((r.try_get::<_, String>("collection")?, row_to_chunk(r)?)))
            .collect()
    }

    /// **写穿**（B6 §2）：把某 chunk 的向量写回 PG `embedding` 列（直查档的向量由此进 PG），
    /// 同时记 `embed_model`（来源/版本）+ 刷 `updated_at`。向量以 `$1::text::vector` 文本传
    /// （免 pgvector ToSql 依赖）。返回更新行数。
    ///
    /// **幂等守卫**：`AND (embedding IS DISTINCT FROM $1 OR embed_model IS DISTINCT FROM $5)`
    /// ——值未变 → 0 行更新 → **不产生复制事件**，即便某部署的 publication 未排除派生列也能阻尼
    /// CDC 写穿反馈环（与列清单 publication 互为防线）。`embedding`/`embed_model`/`updated_at`
    /// 三列已不在 publication 列清单 → 正常情况下这条 UPDATE 本就不复制。
    pub async fn set_embedding(
        &self,
        collection: &str,
        doc_id: &str,
        chunk_id: u64,
        embedding: &[f32],
        model: &str,
    ) -> Result<u64> {
        let sql = format!(
            "UPDATE {} SET embedding = $1::text::vector, embed_model = $5, updated_at = now() \
             WHERE collection = $2 AND doc_id = $3 AND chunk_id = $4 \
             AND (embedding IS DISTINCT FROM $1::text::vector OR embed_model IS DISTINCT FROM $5)",
            self.cfg.table
        );
        let v = format_vector(embedding);
        Ok(self
            .client
            .execute(
                &sql,
                &[&v, &collection, &doc_id, &(chunk_id as i64), &model],
            )
            .await?)
    }

    /// 清除某 chunk 的 `embedding`（设 NULL）+ 清 `embed_model`：B6 写穿路径下，chunk 文本变空
    /// （如媒资丢 caption）时删其向量，避免直查命中残留。仅在 `embedding` 非空时更新（幂等、不空转复制）。
    pub async fn clear_embedding(
        &self,
        collection: &str,
        doc_id: &str,
        chunk_id: u64,
    ) -> Result<u64> {
        let sql = format!(
            "UPDATE {} SET embedding = NULL, embed_model = NULL, updated_at = now() \
             WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 AND embedding IS NOT NULL",
            self.cfg.table
        );
        Ok(self
            .client
            .execute(&sql, &[&collection, &doc_id, &(chunk_id as i64)])
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

/// 校验 SQL 标识符（表名）：`[A-Za-z_][A-Za-z0-9_]*`，长度 ≤63（PG 上限）。
/// 表名在 `sql.rs` 经 `format!` 拼进 SQL（值用参数化、标识符不能参数化），故在此把关。
fn validate_identifier(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(PgError::Config(format!(
            "invalid table identifier: {name:?}"
        )))
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
    /// 解析出的媒资引用（供 Citation.media / Citation.time；time 后过滤的权威源）。
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
            // 后过滤读**权威源** media.time（与 brute/HNSW 侧一致）。下推侧对 time 列用
            // `OR col IS NULL` 保证超集（见 sql.rs WhereBuilder）→ 即便列 NULL 而 media.time 有值，
            // 该行仍被下推保留、由此处精确判定，不漏召回（守不变量 #5）。
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
        media_bytes: r.try_get("media_bytes")?,
        image_vector_status: r.try_get("image_vector_status")?,
        time_start_ms: r.try_get("time_start_ms")?,
        time_end_ms: r.try_get("time_end_ms")?,
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
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
        }
    }

    #[test]
    fn rejects_bad_table_identifiers() {
        assert!(validate_identifier("fastsearch_chunks").is_ok());
        assert!(validate_identifier("t1").is_ok());
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("1abc").is_err()); // 数字开头
        assert!(validate_identifier("a;DROP TABLE x").is_err());
        assert!(validate_identifier("a b").is_err());
        assert!(validate_identifier("a\"b").is_err());
    }

    /// 并发 boot 回归（需 DATABASE_URL）：8 个连接同时对**同一 schema** `ensure_schema`，
    /// 事务级 advisory lock 串行化 DDL → 全部成功、无 "tuple concurrently updated"/重复键竞态
    /// （守多副本并发 boot；此前并发 DDL 会非确定失败）。
    #[tokio::test]
    async fn ensure_schema_concurrent_no_race() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip ensure_schema_concurrent_no_race: DATABASE_URL not set");
            return;
        };
        let mut handles = Vec::new();
        for _ in 0..8 {
            let url = url.clone();
            handles.push(tokio::spawn(async move {
                let mut cfg = PgConfig::new(url);
                cfg.table = "fastsearch_chunks_it".into();
                let store = PgStore::connect(cfg).await.expect("connect");
                store.ensure_schema().await
            }));
        }
        for h in handles {
            h.await
                .expect("join")
                .expect("并发 ensure_schema 应成功（advisory lock 串行化 DDL）");
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

    /// MM2c-bytes 集成：inline 字节经 PG `media_bytes` bytea 真源往返（需 DATABASE_URL）。
    #[tokio::test]
    async fn integration_media_bytes_roundtrip() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_media_bytes_roundtrip: DATABASE_URL not set");
            return;
        };
        use fastsearch_core::{AssetPointer, MediaRef};
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_mbytes_it".into();
        let mut store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .batch_execute("DROP TABLE IF EXISTS fastsearch_mbytes_it")
            .await
            .ok();
        store.ensure_schema().await.expect("schema");

        // 一张带 inline 字节的小图 + 一个无字节的文本段。
        let mut img = sample("d.pdf", 1);
        img.kind = ChunkKind::Image;
        img.text = String::new();
        img.media = Some(MediaRef {
            asset: AssetPointer::Inline,
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        let bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]; // PNG 头样例
        img.media_bytes = Some(bytes.clone());
        let txt = sample("d.pdf", 2); // media_bytes None

        store
            .upsert_doc("kb", "d.pdf", &[img, txt])
            .await
            .expect("upsert");

        // 网关按需直查字节：图有字节、文本段无字节、不存在返回 None。
        let got = store
            .fetch_media_bytes("kb", "d.pdf", 1)
            .await
            .expect("fetch bytes");
        assert_eq!(got, Some(bytes));
        let none = store
            .fetch_media_bytes("kb", "d.pdf", 2)
            .await
            .expect("fetch none");
        assert_eq!(none, None);
        let missing = store
            .fetch_media_bytes("kb", "d.pdf", 999)
            .await
            .expect("fetch missing");
        assert_eq!(missing, None);

        // fetch_doc 往返也带回字节（写侧 Chunk.media_bytes 一致）。
        let back = store.fetch_doc("kb", "d.pdf").await.expect("fetch_doc");
        assert!(back
            .iter()
            .find(|c| c.chunk_id == 1)
            .unwrap()
            .media_bytes
            .is_some());

        store.delete_doc("kb", "d.pdf").await.ok();
    }

    /// 不变量 #5 回归（需 DATABASE_URL）：行有 `media.time` 但反规范化列 `time_start_ms` 为
    /// NULL（模拟遗留/外部写入）时，时间区间过滤**仍正确**——下推 `OR IS NULL` 保超集、后过滤读
    /// 权威 `media.time`：匹配的不漏召回、不匹配的精确排除。
    #[tokio::test]
    async fn integration_time_filter_null_column_superset() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_time_filter_null_column_superset: DATABASE_URL not set");
            return;
        };
        use fastsearch_core::{AssetPointer, FieldValue, Filter, MediaRef, TimeSpan};
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_time_null_it".into();
        cfg.vector_dim = 2;
        cfg.vector_type = VectorType::Vector;
        let mut store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .batch_execute("DROP TABLE IF EXISTS fastsearch_time_null_it")
            .await
            .ok();
        store.ensure_schema().await.expect("schema");

        // 音频段：media.time=[2000,4000]，正常写入会同时填 time_start_ms 列。
        let mut c = sample("a.mp3", 1);
        c.kind = ChunkKind::Audio;
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://b/a.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: Some(TimeSpan {
                start_ms: 2000,
                end_ms: 4000,
            }),
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        c.tenant = None;
        c.acl = vec!["public".into()];
        store.upsert_doc("kb", "a.mp3", &[c]).await.expect("upsert");
        store
            .set_embedding("kb", "a.mp3", 1, &[1.0, 0.0], "test")
            .await
            .expect("emb");
        // 模拟"遗留行"：把反规范化列清成 NULL（media.time 仍在 jsonb）。
        store
            .client
            .execute(
                "UPDATE fastsearch_time_null_it SET time_start_ms = NULL, time_end_ms = NULL",
                &[],
            )
            .await
            .expect("null out cols");

        let q = vec![1.0f32, 0.0];
        // 匹配查询（media.time.start=2000 >= 1000）：列 NULL 但**不漏召回**（OR IS NULL + 后过滤）。
        let f_hit = Filter::Gte("time_start_ms".into(), FieldValue::Int(1000));
        let hits = store
            .vector_search(&q, 5, 4, None, Some(&f_hit))
            .await
            .expect("search hit");
        assert_eq!(
            hits.len(),
            1,
            "媒资 time 匹配 → 不漏召回（列 NULL 不致排除）"
        );
        // 不匹配查询（start=2000 < 3000）：后过滤读权威 media.time **精确排除**。
        let f_miss = Filter::Gte("time_start_ms".into(), FieldValue::Int(3000));
        let none = store
            .vector_search(&q, 5, 4, None, Some(&f_miss))
            .await
            .expect("search miss");
        assert!(none.is_empty(), "media.time 不匹配 → 后过滤精确排除");

        store
            .client
            .batch_execute("DROP TABLE fastsearch_time_null_it")
            .await
            .ok();
    }

    /// B6 写穿——`set_embedding` 幂等守卫（断 CDC 反馈环的行为基元；需 DATABASE_URL）。
    /// 相同向量+模型重写 → 0 行更新 → PG **不产生复制事件**，即便部署缺列清单 publication 也阻尼反馈环
    /// （列清单 publication 排除派生列的结构性证明见纯函数单测 `ddl_has_extension_table_publication`）。
    #[tokio::test]
    async fn b6_set_embedding_idempotent_guard() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip b6_set_embedding_idempotent_guard: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_b6_guard_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let mut store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");
        // 再跑一次确认 DDL 幂等（publication 已存在 → 不抢占分支）。
        store.ensure_schema().await.expect("schema idempotent");

        let c = sample("d.pdf", 1);
        store.upsert_doc("kb", "d.pdf", &[c]).await.expect("upsert");
        let v = [0.0f32, 1.0, 0.0, 0.0];
        let n1 = store
            .set_embedding("kb", "d.pdf", 1, &v, "m@4")
            .await
            .expect("set1");
        assert_eq!(n1, 1, "首次写穿应更新 1 行");
        let n2 = store
            .set_embedding("kb", "d.pdf", 1, &v, "m@4")
            .await
            .expect("set2");
        assert_eq!(
            n2, 0,
            "相同向量+模型重写应 0 行（幂等，不空转复制 → 断 CDC 反馈环）"
        );
        // 换模型标记 → 应更新（即便向量同，溯源变）。
        let n3 = store
            .set_embedding("kb", "d.pdf", 1, &v, "m2@4")
            .await
            .expect("set3");
        assert_eq!(n3, 1, "换 embed_model 应更新 1 行");
        // clear_embedding 幂等：清一次更新、再清 0 行。
        let c1 = store.clear_embedding("kb", "d.pdf", 1).await.expect("clr1");
        assert_eq!(c1, 1, "清向量应更新 1 行");
        let c2 = store.clear_embedding("kb", "d.pdf", 1).await.expect("clr2");
        assert_eq!(c2, 0, "已 NULL 再清应 0 行（幂等）");

        store
            .client
            .batch_execute("DROP TABLE fastsearch_b6_guard_it CASCADE")
            .await
            .ok();
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
