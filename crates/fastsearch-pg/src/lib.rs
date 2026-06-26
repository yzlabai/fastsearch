//! # fastsearch-pg
//!
//! Postgres 真源接入：幂等 schema/DDL、Chunk↔行映射、doc_id 级替换写路径、读取。
//! 仅依赖 pgvector + 逻辑复制，**不要求任何 `shared_preload_libraries` 原生扩展**
//! （托管 PG 可移植，见需求 N1b）。详见 [spec](../../docs/specs/12-pg.md)。

mod error;
mod sql;

pub use error::{PgError, Result};
pub use sql::{ChunkRow, VectorType, COLUMNS, PUBLICATION};

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
}
