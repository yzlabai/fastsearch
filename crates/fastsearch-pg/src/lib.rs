//! # fastsearch-pg
//!
//! Postgres 真源接入：幂等 schema/DDL、Chunk↔行映射、doc_id 级替换写路径、读取。
//! 仅依赖 pgvector + 逻辑复制，**不要求任何 `shared_preload_libraries` 原生扩展**
//! （托管 PG 可移植，见需求 N1b）。详见 [spec](../../docs/specs/12-pg.md)。

mod error;
mod jobs;
mod sql;

pub use error::{PgError, Result};
pub use jobs::{
    retry_backoff_ms, DocumentListOptions, DocumentSummary, FailureDisposition, IngestJob,
    IngestMetrics, IngestState, JobLease, JobStore, NewIngestJob, PublicationResult,
    UploadDisposition, UploadResolution,
};
pub use sql::{
    ann_index_sql, pgvector_search_sql, ChunkRow, SignalRow, SqlParam, VectorType, COLUMNS,
    PUBLICATION,
};

use fastsearch_core::{AclFilter, AssetPointer, Chunk, GlobalId, MediaRef, Signal, SignalType};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::Mutex;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, Row, Transaction};

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

    /// 覆盖向量维度（`embedding` 列类型 `halfvec(dim)`）。**须与运行时 embedder 输出维度一致**，
    /// 否则 `set_embedding` 写穿每行都因 pgvector 维度不符报错、embedding 列永空（M18）。
    pub fn with_vector_dim(mut self, dim: usize) -> Self {
        self.vector_dim = dim;
        self
    }
}

/// `ensure_schema` 的事务级 advisory lock key（固定常量，全副本同值才能互斥）。任意 i64；
/// 取自 ASCII `"fss_ddl\0"` 的高位字节，避免与运维自用 advisory key 偶然撞号。
pub(crate) const SCHEMA_DDL_LOCK_KEY: i64 = 0x6673_735f_6464_6c00;

/// Postgres 真源句柄。
///
/// # 为什么 `client` 是 `Mutex<Client>`
///
/// `tokio_postgres::Client::query/execute/batch_execute` 都只借 `&self`（内部走 channel 与连接
/// task 通信，`Client` 本身 `Send + Sync`），唯独 `transaction()` 要 `&mut self`（类型系统强制
/// 同一时刻只能有一个活跃事务）。`upsert_doc` 走事务，又要支持 `Arc<PgStore>` 共享
/// （server 端 `/v1/index` 与 engine 都持有同一 Arc；不再走 `Arc::try_unwrap` + 重连的丑陋路径），
/// 所以必须借 `Mutex` 拿到独占借用。
///
/// ## 性能影响
///
/// **读路径**（`vector_search`/`fetch_inline_bytes`/`fetch_doc` 等）全部经由 engine 的
/// `engine.lock()` 串行化——同一时刻只有一个 search 在跑，Mutex 在搜索路径上实质**无竞争**。
/// 唯一真实竞争：`/v1/index` 的 `upsert_doc` 不持 engine.lock()，与并发的 search 抢
/// Mutex。但此竞争恰好是**期望行为**：写事务进行中不该让 search 看到半写状态。
///
/// ## 后续演进
///
/// 若未来需要真并发读写（多 search 并行 + 不阻塞 ingest），迁移到连接池（`deadpool-postgres`
/// / `bb8`）：读走池化连接，写事务独占一个连接，彻底解除 Mutex 串行化。当前单连接 + Mutex
/// 是"正确性优先、最小依赖"的 v1 选择。
pub struct PgStore {
    client: Mutex<Client>,
    cfg: PgConfig,
    signal_table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedCollection {
    pub ids: Vec<GlobalId>,
    pub object_uris: Vec<String>,
}

/// 一批 CDC 嵌入写穿中的单项操作。整批经 [`PgStore::write_embeddings_atomically`]
/// 在一个数据库事务内提交，任一项失败则全部回滚。
#[derive(Debug, Clone)]
pub enum EmbeddingWrite {
    Set {
        gid: GlobalId,
        embedding: Vec<f32>,
        model: String,
    },
    Clear {
        gid: GlobalId,
    },
}

/// One source-of-truth signal candidate returned by [`PgStore::signal_vector_search`].
#[derive(Debug, Clone, PartialEq)]
pub struct SignalRecallHit {
    pub scored: fastsearch_core::Scored,
    pub citation: fastsearch_core::Citation,
    pub model: Option<String>,
    pub model_version: Option<String>,
}

/// A stable named recall route produced from one signal type.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalRecallList {
    pub source: String,
    pub items: Vec<SignalRecallHit>,
}

impl PgStore {
    /// 连接（后台驱动连接 future）。表名经标识符校验后才用于 SQL 拼接（防御性：表名是运维配置、
    /// 非客户端输入，但若未来被外部影响，此校验阻断注入面）。
    pub async fn connect(cfg: PgConfig) -> Result<Self> {
        validate_identifier(&cfg.table)?;
        let signal_table = signal_table_name(&cfg.table)?;
        let (client, connection) = tokio_postgres::connect(&cfg.url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("fastsearch-pg connection error: {e}");
            }
        });
        Ok(PgStore {
            client: Mutex::new(client),
            cfg,
            signal_table,
        })
    }

    async fn client(&self) -> Result<tokio::sync::MutexGuard<'_, Client>> {
        let mut client = self.client.lock().await;
        if client.is_closed() {
            let (replacement, connection) = tokio_postgres::connect(&self.cfg.url, NoTls).await?;
            tokio::spawn(async move {
                if let Err(error) = connection.await {
                    eprintln!("fastsearch-pg replacement connection error: {error}");
                }
            });
            *client = replacement;
        }
        Ok(client)
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
        self.client().await?.batch_execute(&batch).await?;
        self.ensure_embedding_type().await?;
        Ok(())
    }

    async fn ensure_embedding_type(&self) -> Result<()> {
        let actual = self
            .client()
            .await?
            .query_opt(
                "SELECT format_type(a.atttypid, a.atttypmod) AS embedding_type \
                 FROM pg_attribute a \
                 WHERE a.attrelid = to_regclass($1) \
                   AND a.attname = 'embedding' \
                   AND a.attnum > 0 \
                   AND NOT a.attisdropped",
                &[&self.cfg.table],
            )
            .await?
            .ok_or_else(|| {
                PgError::Mapping(format!(
                    "table '{}' has no embedding column",
                    self.cfg.table
                ))
            })?
            .try_get::<_, String>("embedding_type")?;
        let expected = format!(
            "{}({})",
            match self.cfg.vector_type {
                VectorType::Vector => "vector",
                VectorType::HalfVec => "halfvec",
            },
            self.cfg.vector_dim
        );
        if actual != expected {
            return Err(PgError::Config(format!(
                "table '{}' embedding type is {actual}, but runtime expects {expected}; use a matching FASTSEARCH_VECTOR_DIM or migrate/recreate the table",
                self.cfg.table
            )));
        }
        Ok(())
    }

    /// doc_id 级替换：事务内先删后批量插，保证原子（CDC 看到 delete+insert）。
    pub async fn upsert_doc(
        &self,
        collection: &str,
        doc_id: &str,
        chunks: &[Chunk],
    ) -> Result<u64> {
        let first = chunks.first().ok_or_else(|| {
            PgError::Config(
                "upsert_doc requires at least one chunk; use delete_doc to remove a document"
                    .into(),
            )
        })?;
        if chunks
            .iter()
            .any(|chunk| chunk.doc_id != doc_id || chunk.tenant != first.tenant)
        {
            return Err(PgError::Config(
                "all chunks must match doc_id and tenant for an atomic document replace".into(),
            ));
        }
        let del = sql::delete_doc_sql(&self.cfg.table);
        let ins = sql::insert_sql(&self.cfg.table);
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        // GlobalId deliberately excludes tenant. Serialize every replacement for this coordinate,
        // then reject a different owner before DELETE so one tenant can never replace another's
        // same-named document.
        tx.query_one(sql::lock_document_coordinate_sql(), &[&collection, &doc_id])
            .await?;
        let owners = tx
            .query(
                &sql::lock_doc_ownership_sql(&self.cfg.table),
                &[&collection, &doc_id],
            )
            .await?;
        if owners.iter().any(|row| {
            row.try_get::<_, Option<String>>("tenant")
                .map(|tenant| tenant != first.tenant)
                .unwrap_or(true)
        }) {
            tx.rollback().await?;
            return Err(PgError::Conflict(format!(
                "document {collection}/{doc_id} is owned by another tenant"
            )));
        }
        tx.execute(&del, &[&collection, &doc_id]).await?;
        let mut n = 0u64;
        for c in chunks {
            let row = ChunkRow::from_chunk(collection, c)?;
            let params: [&(dyn ToSql + Sync); 20] = [
                &row.collection,
                &row.doc_id,
                &row.chunk_id,
                &row.kind,
                &row.text,
                &row.metadata,
                &row.searchable,
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
        let chunk_ids: Vec<i64> = chunks.iter().map(|chunk| chunk.chunk_id as i64).collect();
        tx.execute(
            &sql::reconcile_doc_signals_sql(&self.signal_table),
            &[&collection, &doc_id, &chunk_ids],
        )
        .await?;
        stale_doc_signals(&tx, &self.signal_table, &self.cfg.table, collection, doc_id).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// 删除某 doc 全部 chunk。
    pub async fn delete_doc(&self, collection: &str, doc_id: &str) -> Result<u64> {
        let del = sql::delete_doc_sql(&self.cfg.table);
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute(
            &sql::delete_doc_signals_sql(&self.signal_table),
            &[&collection, &doc_id],
        )
        .await?;
        let deleted = tx.execute(&del, &[&collection, &doc_id]).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// 读取某 doc 全部 chunk（按 chunk_id 升序）。
    pub async fn fetch_doc(&self, collection: &str, doc_id: &str) -> Result<Vec<Chunk>> {
        let q = sql::fetch_doc_sql(&self.cfg.table);
        let rows = self
            .client()
            .await?
            .query(&q, &[&collection, &doc_id])
            .await?;
        rows.iter().map(row_to_chunk).collect()
    }

    /// Insert or update one named chunk representation. The chunk must already exist; body and
    /// artifact hashes plus `embedding_dim` are derived inside Postgres and caller values for
    /// those fields are ignored. Identical writes return `0` and leave `updated_at` unchanged.
    pub async fn upsert_signal(&self, signal: &Signal) -> Result<u64> {
        if let Some(embedding) = &signal.embedding {
            ensure_finite(embedding)?;
        }
        let row = SignalRow::from_signal(signal);
        let statement = sql::upsert_signal_sql(&self.signal_table, &self.cfg.table);
        let params: [&(dyn ToSql + Sync); 10] = [
            &row.collection,
            &row.doc_id,
            &row.chunk_id,
            &row.signal_type,
            &row.status,
            &row.model,
            &row.model_version,
            &row.signal_text,
            &row.embedding,
            &row.error,
        ];
        let updated = self.client().await?.query_opt(&statement, &params).await?;
        if updated.is_some() {
            return Ok(1);
        }
        if self
            .batch_get(std::slice::from_ref(&signal.gid))
            .await?
            .first()
            .is_some_and(Option::is_none)
        {
            return Err(PgError::Conflict(format!(
                "cannot attach signal to missing chunk {}",
                signal.gid.to_citation_id()
            )));
        }
        Ok(0)
    }

    /// Read all named signals for one chunk in stable `signal_type` order. This is an audit and
    /// rebuild library API; it is not wired into search or any external endpoint in FS-201.
    pub async fn fetch_signals(&self, gid: &GlobalId) -> Result<Vec<Signal>> {
        let statement = sql::fetch_signals_sql(&self.signal_table);
        let rows = self
            .client()
            .await?
            .query(
                &statement,
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await?;
        rows.iter().map(row_to_signal).collect()
    }

    /// Set one signal vector and mark it ready. Identical writes return `0`, preventing empty
    /// updates and future replication feedback even if an operator misconfigures publications.
    pub async fn set_signal_embedding(
        &self,
        gid: &GlobalId,
        signal_type: SignalType,
        embedding: &[f32],
        model: &str,
        model_version: Option<&str>,
    ) -> Result<u64> {
        ensure_finite(embedding)?;
        let statement = sql::set_signal_embedding_sql(&self.signal_table);
        let chunk_id = gid.chunk_id as i64;
        let signal_type = signal_type.as_str();
        let embedding = embedding.to_vec();
        Ok(self
            .client()
            .await?
            .execute(
                &statement,
                &[
                    &gid.collection,
                    &gid.doc_id,
                    &chunk_id,
                    &signal_type,
                    &embedding,
                    &model,
                    &model_version,
                ],
            )
            .await?)
    }

    /// Count signal rows whose parent chunk is missing. Normal PgStore write/delete paths keep
    /// this at zero; the audit remains necessary because the additive table intentionally has no
    /// foreign key and external SQL writers can bypass those paths.
    pub async fn orphan_signal_count(&self) -> Result<usize> {
        let rows = self
            .client()
            .await?
            .query(
                &sql::orphan_signals_sql(&self.signal_table, &self.cfg.table),
                &[],
            )
            .await?;
        Ok(rows.len())
    }

    /// 批量按 GlobalId 读取，返回项与请求严格同序；不存在的项为 None。
    pub async fn batch_get(&self, ids: &[GlobalId]) -> Result<Vec<Option<Chunk>>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let collections: Vec<String> = ids.iter().map(|id| id.collection.clone()).collect();
        let doc_ids: Vec<String> = ids.iter().map(|id| id.doc_id.clone()).collect();
        let chunk_ids: Vec<i64> = ids.iter().map(|id| id.chunk_id as i64).collect();
        let q = sql::batch_get_sql(&self.cfg.table);
        let rows = self
            .client()
            .await?
            .query(&q, &[&collections, &doc_ids, &chunk_ids])
            .await?;
        rows.iter()
            .map(
                |row| match row.try_get::<_, Option<String>>("collection")? {
                    Some(_) => Ok(Some(row_to_chunk(row)?)),
                    None => Ok(None),
                },
            )
            .collect()
    }

    /// 跨文档/集合的 chunk 级幂等 upsert，整个 batch 单事务提交。
    ///
    /// 冲突行 tenant 不同则整批失败，避免调用方覆盖其他租户的同 GlobalId。
    pub async fn upsert_chunks(&self, rows: &[(String, Chunk)]) -> Result<u64> {
        let mut document_owners = BTreeMap::<(String, String), Option<String>>::new();
        for (_, chunk) in rows {
            chunk.validate_metadata()?;
        }
        for (collection, chunk) in rows {
            let coordinate = (collection.clone(), chunk.doc_id.clone());
            if let Some(owner) = document_owners.get(&coordinate) {
                if owner != &chunk.tenant {
                    return Err(PgError::Conflict(format!(
                        "document {}/{} has multiple tenants in one batch",
                        collection, chunk.doc_id
                    )));
                }
            } else {
                document_owners.insert(coordinate, chunk.tenant.clone());
            }
        }
        let sql = sql::upsert_chunk_sql(&self.cfg.table);
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        // Acquire the same globally ordered document locks as `upsert_doc`. Sorting prevents two
        // multi-document batches from deadlocking, and checking all existing chunks prevents a
        // different tenant from inserting a new chunk_id into another tenant's document.
        for ((collection, doc_id), owner) in &document_owners {
            tx.query_one(sql::lock_document_coordinate_sql(), &[collection, doc_id])
                .await?;
            let existing = tx
                .query(
                    &sql::lock_doc_ownership_sql(&self.cfg.table),
                    &[collection, doc_id],
                )
                .await?;
            if existing.iter().any(|row| {
                row.try_get::<_, Option<String>>("tenant")
                    .map(|tenant| &tenant != owner)
                    .unwrap_or(true)
            }) {
                tx.rollback().await?;
                return Err(PgError::Conflict(format!(
                    "document {collection}/{doc_id} is owned by another tenant"
                )));
            }
        }
        let mut count = 0u64;
        let mut touched_docs = HashSet::new();
        for (collection, chunk) in rows {
            let row = ChunkRow::from_chunk(collection, chunk)?;
            let params: [&(dyn ToSql + Sync); 20] = [
                &row.collection,
                &row.doc_id,
                &row.chunk_id,
                &row.kind,
                &row.text,
                &row.metadata,
                &row.searchable,
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
            if tx.query_opt(&sql, &params).await?.is_none() {
                return Err(PgError::Conflict(format!(
                    "chunk {}:{}:{} belongs to another tenant",
                    collection, chunk.doc_id, chunk.chunk_id
                )));
            }
            touched_docs.insert((collection.clone(), chunk.doc_id.clone()));
            count += 1;
        }
        let mut touched_docs: Vec<_> = touched_docs.into_iter().collect();
        touched_docs.sort();
        for (collection, doc_id) in touched_docs {
            stale_doc_signals(
                &tx,
                &self.signal_table,
                &self.cfg.table,
                &collection,
                &doc_id,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(count)
    }

    /// 批量删除可见 chunk；结果与请求同序。不存在和未授权统一返回 false。
    pub async fn delete_chunks_visible(
        &self,
        ids: &[GlobalId],
        acl: &AclFilter,
    ) -> Result<Vec<bool>> {
        let sql = sql::delete_chunk_visible_sql(&self.cfg.table, acl.tenant.is_some());
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let mut deleted = Vec::with_capacity(ids.len());
        for id in ids {
            let chunk_id = id.chunk_id as i64;
            let row = if let Some(tenant) = &acl.tenant {
                tx.query_opt(
                    &sql,
                    &[
                        &id.collection,
                        &id.doc_id,
                        &chunk_id,
                        tenant,
                        &acl.allowed_tags,
                    ],
                )
                .await?
            } else {
                tx.query_opt(
                    &sql,
                    &[&id.collection, &id.doc_id, &chunk_id, &acl.allowed_tags],
                )
                .await?
            };
            if row.is_some() {
                delete_signals_for_chunk(&tx, &self.signal_table, id).await?;
            }
            deleted.push(row.is_some());
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// 文档内按 chunk_id 升序分页，只返回调用方可见行。
    pub async fn list_doc_chunks(
        &self,
        collection: &str,
        doc_id: &str,
        after_chunk_id: Option<u64>,
        limit: usize,
        acl: &AclFilter,
    ) -> Result<Vec<Chunk>> {
        let q = sql::list_doc_chunks_sql(&self.cfg.table, acl.tenant.is_some());
        let after = after_chunk_id.map_or(-1, |id| id as i64);
        let limit = limit as i64;
        let client = self.client().await?;
        let rows = if let Some(tenant) = &acl.tenant {
            client
                .query(
                    &q,
                    &[
                        &collection,
                        &doc_id,
                        &after,
                        tenant,
                        &acl.allowed_tags,
                        &limit,
                    ],
                )
                .await?
        } else {
            client
                .query(
                    &q,
                    &[&collection, &doc_id, &after, &acl.allowed_tags, &limit],
                )
                .await?
        };
        rows.iter().map(row_to_chunk).collect()
    }

    /// 幂等删除一个 owner scope 内的 collection，返回实际删除的 GID 与受管对象 URI。
    pub async fn delete_collection(
        &self,
        collection: &str,
        owner_tenant: Option<&str>,
    ) -> Result<DeletedCollection> {
        let q = sql::delete_collection_sql(&self.cfg.table, owner_tenant.is_some());
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let rows = match owner_tenant {
            Some(tenant) => tx.query(&q, &[&collection, &tenant]).await?,
            None => tx.query(&q, &[&collection]).await?,
        };
        let mut ids = Vec::with_capacity(rows.len());
        let mut object_uris = HashSet::new();
        for row in rows {
            let gid = GlobalId {
                collection: row.try_get("collection")?,
                doc_id: row.try_get("doc_id")?,
                chunk_id: row.try_get::<_, i64>("chunk_id")? as u64,
            };
            delete_signals_for_chunk(&tx, &self.signal_table, &gid).await?;
            ids.push(gid);
            if let Some(media_json) = row.try_get::<_, Option<String>>("media")? {
                let media: MediaRef = serde_json::from_str(&media_json)?;
                collect_object_uri(&media.asset, &mut object_uris);
                if let Some(thumbnail) = &media.thumbnail {
                    collect_object_uri(thumbnail, &mut object_uris);
                }
            }
        }
        tx.commit().await?;
        let mut object_uris: Vec<String> = object_uris.into_iter().collect();
        object_uris.sort();
        Ok(DeletedCollection { ids, object_uris })
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
            .client()
            .await?
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
        let rows = self.client().await?.query(&q, &[]).await?;
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
        // 非有限值（NaN/inf，如嵌入服务异常返回）会被 pgvector 以晦涩错误拒收——提前给清晰错误。
        ensure_finite(embedding)?;
        let sql = format!(
            "UPDATE {} SET embedding = $1::text::vector, embed_model = $5, updated_at = now() \
             WHERE collection = $2 AND doc_id = $3 AND chunk_id = $4 \
             AND (embedding IS DISTINCT FROM $1::text::vector OR embed_model IS DISTINCT FROM $5)",
            self.cfg.table
        );
        let v = format_vector(embedding);
        Ok(self
            .client()
            .await?
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
            .client()
            .await?
            .execute(&sql, &[&collection, &doc_id, &(chunk_id as i64)])
            .await?)
    }

    /// 原子执行一批 embedding set/clear。所有向量先完成有限值校验，再开启事务；事务中任一
    /// UPDATE 失败会显式 rollback，避免 CDC 批次只写穿前半部分。
    pub async fn write_embeddings_atomically(&self, writes: &[EmbeddingWrite]) -> Result<Vec<u64>> {
        for write in writes {
            if let EmbeddingWrite::Set { embedding, .. } = write {
                ensure_finite(embedding)?;
            }
        }
        if writes.is_empty() {
            return Ok(Vec::new());
        }

        let set_sql = format!(
            "UPDATE {} SET embedding = $1::text::vector, embed_model = $5, updated_at = now() \
             WHERE collection = $2 AND doc_id = $3 AND chunk_id = $4 \
             AND (embedding IS DISTINCT FROM $1::text::vector OR embed_model IS DISTINCT FROM $5)",
            self.cfg.table
        );
        let clear_sql = format!(
            "UPDATE {} SET embedding = NULL, embed_model = NULL, updated_at = now() \
             WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 AND embedding IS NOT NULL",
            self.cfg.table
        );
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let mut counts = Vec::with_capacity(writes.len());
        for write in writes {
            let result = match write {
                EmbeddingWrite::Set {
                    gid,
                    embedding,
                    model,
                } => {
                    let vector = format_vector(embedding);
                    tx.execute(
                        &set_sql,
                        &[
                            &vector,
                            &gid.collection,
                            &gid.doc_id,
                            &(gid.chunk_id as i64),
                            model,
                        ],
                    )
                    .await
                }
                EmbeddingWrite::Clear { gid } => {
                    tx.execute(
                        &clear_sql,
                        &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
                    )
                    .await
                }
            };
            match result {
                Ok(count) => counts.push(count),
                Err(error) => {
                    tx.rollback().await?;
                    return Err(error.into());
                }
            }
        }
        tx.commit().await?;
        Ok(counts)
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
        ensure_finite(query)?; // NaN/inf 查询向量提前拦下（清晰错误，避免 pgvector 晦涩拒收）。
        let limit = k.saturating_mul(over_fetch.max(1)).max(k);
        let (sql, sparams) =
            pgvector_search_sql(&self.cfg.table, self.cfg.vector_type, limit, acl, filter);
        // filter-aware：iterative scan + 提高 ef_search（会话级，对本直查连接生效）。
        let client = self.client().await?;
        client
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
        let rows = client.query(&sql, &params).await?;

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

    /// Exact multi-representation recall from the signal source of truth. Only ready,
    /// dimension-compatible, searchable rows participate. ACL/filter use the same SQL-superset +
    /// Rust-exact contract as [`Self::vector_search`]. Results are grouped by stable public source
    /// name and each route is independently capped at `k`.
    pub async fn signal_vector_search(
        &self,
        query: &[f32],
        k: usize,
        over_fetch: usize,
        acl: Option<&fastsearch_core::AclFilter>,
        filter: Option<&fastsearch_core::Filter>,
    ) -> Result<Vec<SignalRecallList>> {
        if k == 0 || query.is_empty() {
            return Ok(Vec::new());
        }
        ensure_finite(query)?;
        let limit = k.saturating_mul(over_fetch.max(1)).max(k);
        let (statement, sql_params) =
            sql::signal_vector_search_sql(&self.signal_table, &self.cfg.table, limit, acl, filter);
        let query_text = format_vector(query);
        let query_dim = i32::try_from(query.len())
            .map_err(|_| PgError::Config("query vector dimension exceeds i32".into()))?;
        let signal_types = vec![
            SignalType::Ocr.as_str().to_string(),
            SignalType::Asr.as_str().to_string(),
            SignalType::VlmCaption.as_str().to_string(),
            SignalType::ImageBytes.as_str().to_string(),
        ];
        let mut owned: Vec<Box<dyn ToSql + Sync>> = vec![
            Box::new(query_text),
            Box::new(query_dim),
            Box::new(signal_types),
        ];
        for param in &sql_params {
            owned.push(match param {
                SqlParam::Text(value) => Box::new(value.clone()),
                SqlParam::Int(value) => Box::new(*value),
                SqlParam::TextArray(values) => Box::new(values.clone()),
            });
        }
        let params: Vec<&(dyn ToSql + Sync)> = owned
            .iter()
            .map(|value| &**value as &(dyn ToSql + Sync))
            .collect();
        let client = self.client().await?;
        let rows = client.query(&statement, &params).await?;

        let mut grouped = std::collections::BTreeMap::<String, Vec<SignalRecallHit>>::new();
        for row in &rows {
            let chunk = PgVecRow::from_row(row)?;
            if filter.is_some_and(|candidate_filter| !candidate_filter.eval(&chunk))
                || acl.is_some_and(|candidate_acl| !candidate_acl.visible(&chunk))
            {
                continue;
            }
            let source = match row.try_get::<_, String>("signal_type")?.as_str() {
                "ocr" => "vector:ocr",
                "asr" => "vector:asr",
                "vlm_caption" => "vector:image_caption",
                "image_bytes" => "vector:image_bytes",
                _ => continue,
            };
            grouped
                .entry(source.into())
                .or_default()
                .push(SignalRecallHit {
                    scored: fastsearch_core::Scored {
                        id: chunk.gid(),
                        score: row.try_get("score")?,
                    },
                    citation: chunk.citation(),
                    model: row.try_get("model")?,
                    model_version: row.try_get("model_version")?,
                });
        }
        Ok(grouped
            .into_iter()
            .map(|(source, mut items)| {
                items.sort_by(|left, right| {
                    right
                        .scored
                        .score
                        .partial_cmp(&left.scored.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| left.scored.id.cmp(&right.scored.id))
                });
                items.truncate(k);
                SignalRecallList { source, items }
            })
            .collect())
    }
}

async fn stale_doc_signals(
    tx: &Transaction<'_>,
    signal_table: &str,
    chunks_table: &str,
    collection: &str,
    doc_id: &str,
) -> Result<()> {
    let body_bound: Vec<&str> = SignalType::ALL
        .into_iter()
        .filter(|signal_type| signal_type.binds_body())
        .map(SignalType::as_str)
        .collect();
    let artifact_bound: Vec<&str> = SignalType::ALL
        .into_iter()
        .filter(|signal_type| signal_type.binds_artifact())
        .map(SignalType::as_str)
        .collect();
    tx.execute(
        &sql::stale_body_bound_signals_sql(signal_table, chunks_table),
        &[&collection, &doc_id, &body_bound],
    )
    .await?;
    tx.execute(
        &sql::stale_artifact_bound_signals_sql(signal_table, chunks_table),
        &[&collection, &doc_id, &artifact_bound],
    )
    .await?;
    Ok(())
}

async fn delete_signals_for_chunk(
    tx: &Transaction<'_>,
    signal_table: &str,
    gid: &GlobalId,
) -> Result<()> {
    tx.execute(
        &sql::delete_chunk_signals_sql(signal_table),
        &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
    )
    .await?;
    Ok(())
}

fn collect_object_uri(asset: &AssetPointer, out: &mut HashSet<String>) {
    if let AssetPointer::Object { uri } = asset {
        out.insert(uri.clone());
    }
}

fn row_to_signal(row: &Row) -> Result<Signal> {
    SignalRow {
        collection: row.try_get("collection")?,
        doc_id: row.try_get("doc_id")?,
        chunk_id: row.try_get("chunk_id")?,
        signal_type: row.try_get("signal_type")?,
        status: row.try_get("status")?,
        model: row.try_get("model")?,
        model_version: row.try_get("model_version")?,
        artifact_hash: row.try_get("artifact_hash")?,
        body_hash: row.try_get("body_hash")?,
        signal_text: row.try_get("signal_text")?,
        embedding: row.try_get("embedding")?,
        embedding_dim: row.try_get("embedding_dim")?,
        error: row.try_get("error")?,
    }
    .to_signal()
}

/// 校验 SQL 标识符（表名）：`[A-Za-z_][A-Za-z0-9_]*`，长度 ≤63（PG 上限）。
/// 表名在 `sql.rs` 经 `format!` 拼进 SQL（值用参数化、标识符不能参数化），故在此把关。
pub(crate) fn validate_identifier(name: &str) -> Result<()> {
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

fn signal_table_name(chunks_table: &str) -> Result<String> {
    let signal_table = format!("{chunks_table}_signal");
    validate_identifier(&signal_table)?;
    validate_identifier(&format!("{signal_table}_doc"))?;
    validate_identifier(&format!("{signal_table}_worklist"))?;
    Ok(signal_table)
}

/// f32 向量 → pgvector 文本字面 `[v1,v2,...]`（配合 SQL 内 `$1::text::vector`：先 text 再 vector，
/// 避免 tokio-postgres 把 `$1` 推断成 vector 类型而拒收 String，同 jsonb 写入的处理）。
/// 校验向量全为有限值（无 NaN/inf）。非有限值会被 pgvector 文本解析拒收（晦涩错误）——提前拦下。
fn ensure_finite(v: &[f32]) -> Result<()> {
    if let Some(i) = v.iter().position(|x| !x.is_finite()) {
        return Err(PgError::Mapping(format!(
            "embedding 含非有限值（NaN/inf）于第 {i} 维；嵌入服务返回异常向量"
        )));
    }
    Ok(())
}

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
        metadata: r.try_get("metadata")?,
        searchable: r.try_get("searchable")?,
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

/// 按主键从真源取单个 `(collection, Chunk)`。CDC 遇 `UnchangedToast`（TOAST 大列在 UPDATE 里
/// 未变、WAL 不带值）时，用它从 PG 真源**重取整行**再派生索引（"PG 是真源、索引派生"）。
/// 用调用方的 `&Client`（sync 层复用其连接）；期间行已被删 → `Ok(None)`。`table` 须为安全标识符。
pub async fn fetch_chunk(
    client: &Client,
    table: &str,
    collection: &str,
    doc_id: &str,
    chunk_id: i64,
) -> Result<Option<(String, Chunk)>> {
    let q = sql::fetch_chunk_sql(table);
    let rows = client.query(&q, &[&collection, &doc_id, &chunk_id]).await?;
    match rows.first() {
        Some(r) => Ok(Some((
            r.try_get::<_, String>("collection")?,
            row_to_chunk(r)?,
        ))),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, ChunkKind, SignalStatus};

    #[test]
    fn ensure_finite_rejects_nan_inf() {
        assert!(ensure_finite(&[1.0, 0.5, -0.3]).is_ok());
        assert!(ensure_finite(&[1.0, f32::NAN, 0.0]).is_err());
        assert!(ensure_finite(&[f32::INFINITY, 0.0]).is_err());
    }

    #[test]
    fn pg_config_with_vector_dim_threads_into_ddl() {
        // M18：维度须能从 embedder 联动进 `embedding halfvec(dim)` 列，防硬编码 384 与真实模型维度
        // 不符（否则 set_embedding 逐行维度不匹配、embedding 列永空）。
        let cfg = PgConfig::new("postgres://x").with_vector_dim(768);
        assert_eq!(cfg.vector_dim, 768);
        let ddl = sql::ddl(&cfg.table, cfg.vector_type, cfg.vector_dim).join("\n");
        assert!(
            ddl.contains("embedding halfvec(768)"),
            "列维度应联动: {ddl}"
        );
    }

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
            metadata: Default::default(),
            searchable: true,
        }
    }

    fn database_url_with_name(url: &str, database: &str) -> String {
        let (base, query) = url
            .split_once('?')
            .map_or((url, ""), |(base, query)| (base, query));
        let slash = base.rfind('/').expect("DATABASE_URL must contain a path");
        let suffix = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        format!("{}{database}{suffix}", &base[..=slash])
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

    #[test]
    fn derived_signal_table_identifier_is_validated() {
        assert_eq!(
            signal_table_name("fastsearch_chunks").unwrap(),
            "fastsearch_chunks_signal"
        );
        let valid_parent_but_derived_too_long = format!("a{}", "x".repeat(47));
        assert!(validate_identifier(&valid_parent_but_derived_too_long).is_ok());
        assert!(
            validate_identifier(&format!("{valid_parent_but_derived_too_long}_signal")).is_ok()
        );
        assert!(signal_table_name(&valid_parent_but_derived_too_long).is_err());
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
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_chunks_it".into();
        let store = PgStore::connect(cfg).await.expect("verification connect");
        let signal_exists: bool = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT to_regclass('fastsearch_chunks_it_signal') IS NOT NULL",
                &[],
            )
            .await
            .expect("signal table lookup")
            .get(0);
        assert!(
            signal_exists,
            "concurrent boot must create the derived signal table"
        );
    }

    #[tokio::test]
    async fn fs304_pg_store_reconnects_after_backend_termination() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs304_pg_store_reconnects_after_backend_termination: DATABASE_URL not set"
            );
            return;
        };
        let store = PgStore::connect(PgConfig::new(url.clone()))
            .await
            .expect("connect store");
        let backend_pid: i32 = store
            .client
            .lock()
            .await
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("backend pid")
            .get(0);
        let (killer, connection) = tokio_postgres::connect(&url, NoTls)
            .await
            .expect("killer connect");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        assert!(killer
            .query_one("SELECT pg_terminate_backend($1)", &[&backend_pid])
            .await
            .expect("terminate source connection")
            .get::<_, bool>(0));
        for _ in 0..50 {
            if store.client.lock().await.is_closed() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(store.client.lock().await.is_closed());
        assert_eq!(
            store
                .client()
                .await
                .expect("same PgStore instance reconnects")
                .query_one("SELECT 1::integer", &[])
                .await
                .expect("query after reconnect")
                .get::<_, i32>(0),
            1
        );
    }

    #[tokio::test]
    async fn ensure_schema_rejects_existing_vector_dimension_mismatch() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip ensure_schema_rejects_existing_vector_dimension_mismatch: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fastsearch_chunks_dim_guard_{}", std::process::id());
        let mut initial = PgConfig::new(url.clone()).with_vector_dim(4);
        initial.table = table.clone();
        let store = PgStore::connect(initial).await.expect("connect");
        store.ensure_schema().await.expect("initial schema");

        let mut mismatched = PgConfig::new(url).with_vector_dim(3);
        mismatched.table = table.clone();
        let store = PgStore::connect(mismatched).await.expect("reconnect");
        let error = store
            .ensure_schema()
            .await
            .expect_err("dimension mismatch must fail");
        assert!(
            error.to_string().contains("halfvec(4)") && error.to_string().contains("halfvec(3)"),
            "unexpected error: {error}"
        );

        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE IF EXISTS {table};"))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs302_doc_replace_rejects_cross_tenant_global_id_collision() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs302_doc_replace_rejects_cross_tenant_global_id_collision: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fs302_owner_chunks_{}", std::process::id());
        let mut cfg = PgConfig::new(url.clone());
        cfg.table = table.clone();
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");

        let mut owned = sample("same.md", 1);
        owned.tenant = Some("tenant-a".into());
        owned.acl = vec!["team-a".into()];
        store
            .upsert_doc("kb", "same.md", std::slice::from_ref(&owned))
            .await
            .expect("initial owner");

        let mut foreign = sample("same.md", 1);
        foreign.tenant = Some("tenant-b".into());
        foreign.acl = vec!["team-b".into()];
        let error = store
            .upsert_doc("kb", "same.md", &[foreign])
            .await
            .expect_err("foreign tenant must not replace global id");
        assert!(matches!(error, PgError::Conflict(_)));
        let rows = store.fetch_doc("kb", "same.md").await.expect("fetch owner");
        assert_eq!(rows, vec![owned]);

        let mut foreign_new_chunk = sample("same.md", 2);
        foreign_new_chunk.tenant = Some("tenant-b".into());
        foreign_new_chunk.acl = vec!["team-b".into()];
        let error = store
            .upsert_chunks(&[("kb".into(), foreign_new_chunk)])
            .await
            .expect_err("a new chunk id must not bypass document ownership");
        assert!(matches!(error, PgError::Conflict(_)));

        let mut peer_cfg = PgConfig::new(url);
        peer_cfg.table = table.clone();
        let peer = PgStore::connect(peer_cfg).await.expect("peer connect");
        let mut race_a = sample("race.md", 1);
        race_a.tenant = Some("tenant-a".into());
        race_a.acl = vec!["team-a".into()];
        let mut race_b = sample("race.md", 2);
        race_b.tenant = Some("tenant-b".into());
        race_b.acl = vec!["team-b".into()];
        let replace_rows = vec![race_a];
        let chunk_rows = vec![("kb".into(), race_b)];
        let (replace, chunk_upsert) = tokio::join!(
            store.upsert_doc("kb", "race.md", &replace_rows),
            peer.upsert_chunks(&chunk_rows)
        );
        assert_ne!(replace.is_ok(), chunk_upsert.is_ok());
        let race_rows = store.fetch_doc("kb", "race.md").await.expect("race rows");
        assert!(!race_rows.is_empty());
        assert!(race_rows
            .iter()
            .all(|row| row.tenant == race_rows[0].tenant));

        store
            .client
            .lock()
            .await
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {table} CASCADE; DROP TABLE IF EXISTS {table}_signal CASCADE;"
            ))
            .await
            .expect("cleanup");
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
        let store = PgStore::connect(cfg).await.expect("connect");
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

    /// FS-201 T1–T4: named signals coexist, derived hashes/dimensions round-trip, and body versus
    /// artifact changes stale exactly the exhaustive rule-table subset.
    #[tokio::test]
    async fn fs201_signal_crud_and_invalidation() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip fs201_signal_crud_and_invalidation: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_fs201_signal_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .lock()
            .await
            .batch_execute(
                "DROP TABLE IF EXISTS fastsearch_fs201_signal_it_signal CASCADE; \
                 DROP TABLE IF EXISTS fastsearch_fs201_signal_it CASCADE;",
            )
            .await
            .expect("clean schema");
        store.ensure_schema().await.expect("schema");

        let mut chunk = sample("dir:sub:report.pdf", 1);
        chunk.media_bytes = Some(vec![1, 2, 3]);
        let gid = chunk.global_id("fs201-crud");
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk.clone())])
            .await
            .expect("chunk");

        for (index, signal_type) in SignalType::ALL.into_iter().enumerate() {
            let signal = Signal {
                gid: gid.clone(),
                signal_type,
                status: fastsearch_core::SignalStatus::Ready,
                model: Some(format!("model-{index}")),
                model_version: Some("v1".into()),
                artifact_hash: Some("caller-must-not-win".into()),
                body_hash: Some("caller-must-not-win".into()),
                signal_text: (signal_type != SignalType::ImageBytes)
                    .then(|| "中文表示".to_string()),
                embedding: Some(vec![index as f32, 1.0]),
                embedding_dim: Some(999),
                error: None,
            };
            assert_eq!(
                store.upsert_signal(&signal).await.expect("upsert signal"),
                1
            );
            assert_eq!(store.upsert_signal(&signal).await.expect("idempotent"), 0);
        }

        let initial = store.fetch_signals(&gid).await.expect("fetch signals");
        assert_eq!(initial.len(), 5);
        assert!(initial.iter().all(|signal| signal.embedding_dim == Some(2)));
        assert!(initial.iter().all(|signal| {
            signal.body_hash.as_deref() != Some("caller-must-not-win")
                && signal.artifact_hash.as_deref() != Some("caller-must-not-win")
        }));
        assert_eq!(
            store
                .set_signal_embedding(
                    &gid,
                    SignalType::VlmCaption,
                    &[3.0, 2.0, 1.0],
                    "caption-model",
                    Some("v2"),
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .set_signal_embedding(
                    &gid,
                    SignalType::VlmCaption,
                    &[3.0, 2.0, 1.0],
                    "caption-model",
                    Some("v2"),
                )
                .await
                .unwrap(),
            0,
            "identical vector/provenance must not update"
        );
        let visual_updated_at: Vec<String> = store
            .client
            .lock()
            .await
            .query(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
                   AND signal_type IN ('vlm_caption', 'image_bytes') ORDER BY signal_type",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();

        let unchanged_timestamps: Vec<String> = store
            .client
            .lock()
            .await
            .query(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 ORDER BY signal_type",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk.clone())])
            .await
            .expect("unchanged chunk upsert");
        assert!(store
            .fetch_signals(&gid)
            .await
            .unwrap()
            .iter()
            .all(|signal| signal.status == fastsearch_core::SignalStatus::Ready));
        let after_unchanged: Vec<String> = store
            .client
            .lock()
            .await
            .query(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 ORDER BY signal_type",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            after_unchanged, unchanged_timestamps,
            "unchanged chunk upsert must update zero signal rows"
        );

        chunk.text = "body changed".into();
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk.clone())])
            .await
            .expect("body update");
        let after_body = store.fetch_signals(&gid).await.unwrap();
        for signal in &after_body {
            if signal.signal_type.binds_body() {
                assert_eq!(signal.status, fastsearch_core::SignalStatus::Stale);
                assert!(signal.embedding.is_none());
            } else {
                assert_eq!(signal.status, fastsearch_core::SignalStatus::Ready);
                assert!(signal.embedding.is_some());
            }
        }
        let visual_after: Vec<String> = store
            .client
            .lock()
            .await
            .query(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
                   AND signal_type IN ('vlm_caption', 'image_bytes') ORDER BY signal_type",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            visual_after, visual_updated_at,
            "visual rows must not be touched"
        );

        // Rebuild all signals against the new body/current artifact, then change only the bytes.
        for signal_type in SignalType::ALL {
            let signal = Signal {
                gid: gid.clone(),
                signal_type,
                status: fastsearch_core::SignalStatus::Ready,
                embedding: Some(vec![9.0, 8.0, 7.0]),
                model: Some("rebuilt".into()),
                model_version: Some("v2".into()),
                ..Signal::default()
            };
            store.upsert_signal(&signal).await.unwrap();
        }
        let user_text_before_artifact: String = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
                   AND signal_type = 'user_text'",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .get(0);
        chunk.media_bytes = Some(vec![9, 9, 9]);
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk)])
            .await
            .expect("artifact update");
        let after_artifact = store.fetch_signals(&gid).await.unwrap();
        for signal in after_artifact {
            let expected = if signal.signal_type.binds_artifact() {
                fastsearch_core::SignalStatus::Stale
            } else {
                fastsearch_core::SignalStatus::Ready
            };
            assert_eq!(signal.status, expected, "{:?}", signal.signal_type);
            if signal.signal_type.binds_artifact() {
                assert!(signal.embedding.is_none(), "{:?}", signal.signal_type);
            } else {
                assert!(signal.embedding.is_some(), "{:?}", signal.signal_type);
            }
        }
        let user_text_after_artifact: String = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT updated_at::text FROM fastsearch_fs201_signal_it_signal \
                 WHERE collection = $1 AND doc_id = $2 AND chunk_id = $3 \
                   AND signal_type = 'user_text'",
                &[&gid.collection, &gid.doc_id, &(gid.chunk_id as i64)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            user_text_after_artifact, user_text_before_artifact,
            "artifact-only changes must not touch user_text"
        );

        let missing = Signal {
            gid: GlobalId {
                chunk_id: 999,
                ..gid.clone()
            },
            ..Signal::default()
        };
        assert!(matches!(
            store.upsert_signal(&missing).await,
            Err(PgError::Conflict(_))
        ));
    }

    /// FS-201 T5/T6/T11: doc replacement reconciles removed chunk ids, and every authorized
    /// delete path removes signals in the same transaction without creating a second ACL source.
    #[tokio::test]
    async fn fs201_signal_reconciliation_and_acl_deletes() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip fs201_signal_reconciliation_and_acl_deletes: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_fs201_delete_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .lock()
            .await
            .batch_execute(
                "DROP TABLE IF EXISTS fastsearch_fs201_delete_it_signal CASCADE; \
                 DROP TABLE IF EXISTS fastsearch_fs201_delete_it CASCADE;",
            )
            .await
            .expect("clean schema");
        store.ensure_schema().await.expect("schema");

        let collection = "fs201-delete";
        let mut kept = sample("report.pdf", 1);
        kept.tenant = Some("tenant-a".into());
        kept.acl = vec!["team-a".into()];
        kept.media_bytes = Some(vec![1]);
        let mut removed = sample("report.pdf", 2);
        removed.tenant = Some("tenant-a".into());
        removed.acl = vec!["team-a".into()];
        removed.media_bytes = Some(vec![2]);
        store
            .upsert_doc(collection, "report.pdf", &[kept.clone(), removed.clone()])
            .await
            .unwrap();
        for chunk in [&kept, &removed] {
            store
                .upsert_signal(&Signal {
                    gid: chunk.global_id(collection),
                    signal_type: SignalType::ImageBytes,
                    status: fastsearch_core::SignalStatus::Ready,
                    embedding: Some(vec![1.0, 2.0]),
                    ..Signal::default()
                })
                .await
                .unwrap();
        }

        let kept_gid = kept.global_id(collection);
        let removed_gid = removed.global_id(collection);
        store
            .upsert_doc(collection, "report.pdf", &[kept.clone()])
            .await
            .expect("replace with one chunk");
        assert_eq!(store.fetch_signals(&kept_gid).await.unwrap().len(), 1);
        assert!(store.fetch_signals(&removed_gid).await.unwrap().is_empty());
        assert_eq!(store.orphan_signal_count().await.unwrap(), 0);

        let denied_acl = AclFilter {
            tenant: Some("tenant-b".into()),
            allowed_tags: vec!["team-a".into()],
        };
        assert_eq!(
            store
                .delete_chunks_visible(std::slice::from_ref(&kept_gid), &denied_acl)
                .await
                .unwrap(),
            vec![false]
        );
        assert_eq!(store.fetch_signals(&kept_gid).await.unwrap().len(), 1);
        let allowed_acl = AclFilter {
            tenant: Some("tenant-a".into()),
            allowed_tags: vec!["team-a".into()],
        };
        assert_eq!(
            store
                .delete_chunks_visible(std::slice::from_ref(&kept_gid), &allowed_acl)
                .await
                .unwrap(),
            vec![true]
        );
        assert!(store.fetch_signals(&kept_gid).await.unwrap().is_empty());
        assert_eq!(store.orphan_signal_count().await.unwrap(), 0);

        // Recreate and cover both broader delete paths.
        store
            .upsert_doc(collection, "report.pdf", &[kept.clone()])
            .await
            .unwrap();
        store
            .upsert_signal(&Signal {
                gid: kept_gid.clone(),
                signal_type: SignalType::VlmCaption,
                status: fastsearch_core::SignalStatus::Ready,
                ..Signal::default()
            })
            .await
            .unwrap();
        store.delete_doc(collection, "report.pdf").await.unwrap();
        assert_eq!(store.orphan_signal_count().await.unwrap(), 0);

        store
            .upsert_doc(collection, "report.pdf", &[kept])
            .await
            .unwrap();
        store
            .upsert_signal(&Signal {
                gid: kept_gid,
                signal_type: SignalType::VlmCaption,
                status: fastsearch_core::SignalStatus::Ready,
                ..Signal::default()
            })
            .await
            .unwrap();
        store
            .delete_collection(collection, Some("tenant-a"))
            .await
            .unwrap();
        assert_eq!(store.orphan_signal_count().await.unwrap(), 0);
    }

    /// FS-201 T7/T9/T10: the signal table is portable core PG storage, is removed from the
    /// chunks-only publication by schema reconciliation, and produces no pgoutput changes.
    #[tokio::test]
    async fn fs201_signal_schema_publication_and_cdc_contract() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs201_signal_schema_publication_and_cdc_contract: DATABASE_URL not set"
            );
            return;
        };
        // This test deliberately rewrites the fixed publication. Give it a private database so
        // parallel PG tests cannot deadlock on whichever test table was previously published.
        let database = format!("fastsearch_fs201_pub_{}", std::process::id());
        let (admin, admin_connection) = tokio_postgres::connect(&url, NoTls)
            .await
            .expect("admin connect");
        tokio::spawn(async move {
            let _ = admin_connection.await;
        });
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE);"))
            .await
            .expect("drop stale private database");
        admin
            .batch_execute(&format!("CREATE DATABASE {database};"))
            .await
            .expect("private database");
        let isolated_url = database_url_with_name(&url, &database);
        let mut cfg = PgConfig::new(isolated_url);
        cfg.table = "fastsearch_fs201_pub_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");

        store
            .client
            .lock()
            .await
            .batch_execute(
                "DROP PUBLICATION IF EXISTS fastsearch_pub; \
                 DROP TABLE IF EXISTS fastsearch_fs201_pub_it_signal CASCADE; \
                 DROP TABLE IF EXISTS fastsearch_fs201_pub_it CASCADE;",
            )
            .await
            .expect("clean schema");
        store.ensure_schema().await.expect("schema");

        let columns = store
            .client
            .lock()
            .await
            .query(
                "SELECT column_name, data_type, udt_name FROM information_schema.columns \
                 WHERE table_name = 'fastsearch_fs201_pub_it_signal' ORDER BY ordinal_position",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(columns.len(), 14);
        assert!(!columns.iter().any(|row| {
            matches!(
                row.get::<_, String>("column_name").as_str(),
                "tenant" | "acl"
            )
        }));
        let embedding = columns
            .iter()
            .find(|row| row.get::<_, String>("column_name") == "embedding")
            .unwrap();
        assert_eq!(embedding.get::<_, String>("data_type"), "ARRAY");
        assert_eq!(embedding.get::<_, String>("udt_name"), "_float4");
        let foreign_keys: i64 = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT count(*) FROM information_schema.table_constraints \
                 WHERE table_name = 'fastsearch_fs201_pub_it_signal' \
                   AND constraint_type = 'FOREIGN KEY'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(foreign_keys, 0);

        // An operator adds the sibling table by mistake; ensure_schema must restore one table.
        store
            .client
            .lock()
            .await
            .batch_execute(
                "ALTER PUBLICATION fastsearch_pub ADD TABLE fastsearch_fs201_pub_it_signal;",
            )
            .await
            .expect("misconfigure publication");
        store.ensure_schema().await.expect("reconcile publication");
        let publication_tables: Vec<String> = store
            .client
            .lock()
            .await
            .query(
                "SELECT c.relname FROM pg_publication_rel pr \
                 JOIN pg_publication p ON p.oid = pr.prpubid \
                 JOIN pg_class c ON c.oid = pr.prrelid \
                 WHERE p.pubname = 'fastsearch_pub' ORDER BY c.relname",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(publication_tables, vec!["fastsearch_fs201_pub_it"]);

        let mut chunk = sample("cdc.pdf", 1);
        chunk.media_bytes = Some(vec![1, 2, 3]);
        let gid = chunk.global_id("fs201-cdc");
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk.clone())])
            .await
            .unwrap();
        let slot = format!("fs201_signal_{}", std::process::id());
        store
            .client
            .lock()
            .await
            .execute(
                "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
                 WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .unwrap();
        store
            .client
            .lock()
            .await
            .query_one(
                "SELECT * FROM pg_create_logical_replication_slot($1, 'pgoutput')",
                &[&slot],
            )
            .await
            .expect("slot");
        store
            .upsert_signal(&Signal {
                gid: gid.clone(),
                signal_type: SignalType::VlmCaption,
                status: fastsearch_core::SignalStatus::Ready,
                signal_text: Some("图表说明".into()),
                ..Signal::default()
            })
            .await
            .unwrap();
        let after_signal: i64 = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT count(*) FROM pg_logical_slot_peek_binary_changes( \
                 $1, NULL, NULL, 'proto_version', '1', 'publication_names', 'fastsearch_pub')",
                &[&slot],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            after_signal, 0,
            "signal table must not emit publication changes"
        );
        chunk.text = "normal chunk update".into();
        store
            .upsert_chunks(&[(gid.collection.clone(), chunk)])
            .await
            .unwrap();
        let after_chunk: i64 = store
            .client
            .lock()
            .await
            .query_one(
                "SELECT count(*) FROM pg_logical_slot_peek_binary_changes( \
                 $1, NULL, NULL, 'proto_version', '1', 'publication_names', 'fastsearch_pub')",
                &[&slot],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            after_chunk > 0,
            "normal chunks update must still be published"
        );
        store
            .client
            .lock()
            .await
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await
            .expect("drop slot");
        drop(store);
        admin
            .batch_execute(&format!("DROP DATABASE {database} WITH (FORCE);"))
            .await
            .expect("drop private database");
    }

    #[tokio::test]
    async fn integration_chunk_management_lifecycle() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_chunk_management_lifecycle: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_chunks_it".into();
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");
        let collection = format!("management-{}", std::process::id());
        store
            .delete_collection(&collection, None)
            .await
            .expect("clean");

        let mut first = sample("doc-a", 1);
        first.tenant = Some("tenant-a".into());
        first.acl = vec!["team-a".into()];
        first
            .metadata
            .insert("source".into(), serde_json::json!("first"));
        let mut second = sample("doc-a", 2);
        second.tenant = Some("tenant-a".into());
        second.acl = vec!["public".into()];
        second.searchable = false;
        let mut private = sample("doc-b", 3);
        private.tenant = Some("tenant-b".into());
        private.acl = vec!["team-b".into()];

        assert_eq!(
            store
                .upsert_chunks(&[
                    (collection.clone(), first.clone()),
                    (collection.clone(), second.clone()),
                    (collection.clone(), private.clone()),
                ])
                .await
                .unwrap(),
            3
        );

        let ids = vec![
            first.global_id(&collection),
            GlobalId {
                collection: collection.clone(),
                doc_id: "missing".into(),
                chunk_id: 9,
            },
            second.global_id(&collection),
        ];
        let got = store.batch_get(&ids).await.unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_ref().unwrap().metadata["source"], "first");
        assert!(got[1].is_none());
        assert!(!got[2].as_ref().unwrap().searchable);

        let acl = AclFilter {
            tenant: Some("tenant-a".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let page = store
            .list_doc_chunks(&collection, "doc-a", None, 1, &acl)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].chunk_id, 1);
        let page = store
            .list_doc_chunks(&collection, "doc-a", Some(1), 10, &acl)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].chunk_id, 2);

        let deleted = store
            .delete_chunks_visible(
                &[first.global_id(&collection), private.global_id(&collection)],
                &acl,
            )
            .await
            .unwrap();
        assert_eq!(deleted, vec![true, false]);

        let removed = store
            .delete_collection(&collection, Some("tenant-a"))
            .await
            .unwrap();
        assert_eq!(removed.ids, vec![second.global_id(&collection)]);
        assert!(removed.object_uris.is_empty());
        let other = store
            .batch_get(&[private.global_id(&collection)])
            .await
            .unwrap();
        assert!(other[0].is_some(), "tenant-b row must remain");
        store
            .delete_collection(&collection, None)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn integration_schema_upgrade_adds_metadata_and_searchable() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip integration_schema_upgrade_adds_metadata_and_searchable: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_upgrade_it".into();
        let store = PgStore::connect(cfg).await.expect("connect");
        let ddl = sql::ddl(
            &store.cfg.table,
            store.cfg.vector_type,
            store.cfg.vector_dim,
        );
        let legacy_create = ddl[1]
            .replace("metadata jsonb NOT NULL DEFAULT '{}'::jsonb,\n", "")
            .replace("searchable boolean NOT NULL DEFAULT true,\n", "");
        store
            .client
            .lock()
            .await
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {};\n{}\n{}",
                store.cfg.table, ddl[0], legacy_create
            ))
            .await
            .expect("legacy schema");

        store.ensure_schema().await.expect("upgrade schema");
        let columns = store
            .client
            .lock()
            .await
            .query(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = $1 AND column_name IN ('metadata', 'searchable') \
                 ORDER BY column_name",
                &[&store.cfg.table],
            )
            .await
            .expect("columns");
        assert_eq!(columns.len(), 2);

        let mut c = sample("legacy.pdf", 1);
        c.metadata
            .insert("source".into(), serde_json::json!("upgrade"));
        c.searchable = false;
        store
            .upsert_doc("kb", "legacy.pdf", &[c.clone()])
            .await
            .expect("upsert");
        assert_eq!(
            store.fetch_doc("kb", "legacy.pdf").await.expect("fetch"),
            vec![c]
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
        let store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .lock()
            .await
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
        let store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .lock()
            .await
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
            .lock()
            .await
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
            .lock()
            .await
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
        let store = PgStore::connect(cfg).await.expect("connect");
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
            .lock()
            .await
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
        let store = PgStore::connect(cfg).await.expect("connect");
        // 干净重建表（schema 可能变）。
        store
            .client
            .lock()
            .await
            .batch_execute("DROP TABLE IF EXISTS fastsearch_vec_it")
            .await
            .ok();
        store.ensure_schema().await.expect("schema");
        store
            .client
            .lock()
            .await
            .batch_execute(&sql::ann_index_sql("fastsearch_vec_it", VectorType::Vector))
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
            let v = format_vector(&e);
            let chunk_id = id as i64;
            let params: [&(dyn ToSql + Sync); 2] = [&v, &chunk_id];
            store
                .client
                .lock()
                .await
                .execute(
                    "UPDATE fastsearch_vec_it SET embedding = $1::text::vector WHERE chunk_id = $2",
                    &params,
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
            .lock()
            .await
            .batch_execute("DROP TABLE IF EXISTS fastsearch_vec_it")
            .await
            .ok();
    }

    /// FS-202 T1: the public signal-recall seam exposes only ready, compatible, visible and
    /// searchable signals, grouped under stable source names with provenance.
    #[tokio::test]
    async fn fs202_signal_vector_search_filters_and_orders() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip fs202_signal_vector_search_filters_and_orders: DATABASE_URL not set");
            return;
        };
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_fs202_signal_search_it".into();
        cfg.vector_dim = 2;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");
        store
            .client
            .lock()
            .await
            .batch_execute(
                "DROP TABLE IF EXISTS fastsearch_fs202_signal_search_it_signal CASCADE; \
                 DROP TABLE IF EXISTS fastsearch_fs202_signal_search_it CASCADE;",
            )
            .await
            .expect("clean");
        store.ensure_schema().await.expect("schema");

        let mut chunks: Vec<(String, Chunk)> = (1..=6)
            .map(|id| {
                let mut chunk = sample(
                    if id == 5 {
                        "forbidden.pdf"
                    } else {
                        "signals.pdf"
                    },
                    id,
                );
                chunk.tenant = Some(if id == 5 { "other" } else { "acme" }.into());
                chunk.acl = vec![if id == 5 { "secret" } else { "team-a" }.into()];
                if id == 6 {
                    chunk.searchable = false;
                }
                ("kb".into(), chunk)
            })
            .collect();
        // Make the two equal-score rows observable in GlobalId tie-break order even though their
        // signals are inserted in the opposite order below.
        chunks[0].1.page = 11;
        chunks[1].1.page = 12;
        chunks[1].1.media = Some(fastsearch_core::MediaRef {
            asset: AssetPointer::Inline,
            media_type: Some("audio/wav".into()),
            time: Some(fastsearch_core::TimeSpan {
                start_ms: 1_200,
                end_ms: 2_400,
            }),
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        store.upsert_chunks(&chunks).await.expect("chunks");

        let cases = [
            (2, SignalStatus::Ready, vec![1.0, 0.0], "caption-v2"),
            (1, SignalStatus::Ready, vec![1.0, 0.0], "caption-v1"),
            (3, SignalStatus::Stale, vec![1.0, 0.0], "stale"),
            (4, SignalStatus::Ready, vec![1.0, 0.0, 0.0], "wrong-dim"),
            (5, SignalStatus::Ready, vec![1.0, 0.0], "forbidden"),
            (6, SignalStatus::Ready, vec![1.0, 0.0], "not-searchable"),
        ];
        for (chunk_id, status, embedding, version) in cases {
            store
                .upsert_signal(&Signal {
                    gid: GlobalId {
                        collection: "kb".into(),
                        doc_id: if chunk_id == 5 {
                            "forbidden.pdf".into()
                        } else {
                            "signals.pdf".into()
                        },
                        chunk_id,
                    },
                    signal_type: SignalType::VlmCaption,
                    status,
                    model: Some("clip-caption".into()),
                    model_version: Some(version.into()),
                    signal_text: Some(format!("caption {chunk_id}")),
                    embedding: Some(embedding),
                    ..Default::default()
                })
                .await
                .expect("signal");
        }

        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        assert!(store
            .signal_vector_search(&[], 10, 4, Some(&acl), None)
            .await
            .expect("empty query is an empty recall route")
            .is_empty());
        let lists = store
            .signal_vector_search(&[1.0, 0.0], 10, 4, Some(&acl), None)
            .await
            .expect("signal search");
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].source, "vector:image_caption");
        assert_eq!(
            lists[0]
                .items
                .iter()
                .map(|hit| hit.scored.id.chunk_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(lists[0].items[0].citation.page, 11);
        assert_eq!(lists[0].items[0].model.as_deref(), Some("clip-caption"));
        assert_eq!(
            lists[0].items[0].model_version.as_deref(),
            Some("caption-v1")
        );
        assert_eq!(lists[0].items[0].scored.score, 1.0);

        let page_twelve =
            fastsearch_core::Filter::Eq("page".into(), fastsearch_core::FieldValue::Int(12));
        let filtered = store
            .signal_vector_search(&[1.0, 0.0], 10, 4, Some(&acl), Some(&page_twelve))
            .await
            .expect("filtered signal search");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].items.len(), 1);
        assert_eq!(filtered[0].items[0].scored.id.chunk_id, 2);

        let time_filtered = store
            .signal_vector_search(
                &[1.0, 0.0],
                10,
                4,
                Some(&acl),
                Some(&fastsearch_core::Filter::Gte(
                    "time_start_ms".into(),
                    fastsearch_core::FieldValue::Int(1_000),
                )),
            )
            .await
            .expect("time-filtered signal search");
        assert_eq!(time_filtered.len(), 1);
        assert_eq!(time_filtered[0].items.len(), 1);
        assert_eq!(time_filtered[0].items[0].scored.id.chunk_id, 2);

        store
            .client
            .lock()
            .await
            .batch_execute(
                "DROP TABLE IF EXISTS fastsearch_fs202_signal_search_it_signal CASCADE; \
                 DROP TABLE IF EXISTS fastsearch_fs202_signal_search_it CASCADE;",
            )
            .await
            .ok();
    }
}
