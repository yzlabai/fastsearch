//! # fastsearch-engine
//!
//! 把 core（融合/模型）+ text（全文索引）+ vector（向量后端）+ sync（CDC sink）
//! 整合成端到端引擎：灌入（含经 CDC `IndexSink`）→ 索引 → 排序管线检索 → 带引用命中。
//! 详见 [spec](../../docs/specs/14-engine.md)。
//!
//! 三种检索模式全可用：keyword / vector / **hybrid（keyword∥vector → core::fuse 融合）**。
//! 过滤与 ACL 在两路各自做真预过滤（不可绕过）；分面（kind/doc_id）、高亮、**rerank**
//! （req.rerank 时宽召回后重排）、**auto-merging**（req.auto_merge 同 section 归并）、
//! **分组折叠**（req.collapse 每 doc/section 限 N 条）均已接入。

use fastsearch_core::{
    fuse, AclFilter, AssetPointer, BBox, Chunk, ChunkKind, Citation, GlobalId, Scored, SearchMode,
    SearchRequest, TimeSpan,
};
use fastsearch_embed::{EmbedKind, Embedder};
use fastsearch_rerank::{LexicalOverlapReranker, Reranker};
use fastsearch_sync::replication::{advance_slot, peek_with_lsn, ReplicationConfig};
use fastsearch_sync::{Applier, Lsn};
use fastsearch_text::{TextHit, TextIndex, TextIndexConfig};
pub use fastsearch_vector::{HnswParams, VectorBackendKind};
use fastsearch_vector::{VecMeta, VectorBackend, VectorStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("text index error: {0}")]
    Text(#[from] fastsearch_text::TextError),
    #[error("core error: {0}")]
    Core(#[from] fastsearch_core::CoreError),
    #[error("vector error: {0}")]
    Vector(String),
    #[error("rerank error: {0}")]
    Rerank(String),
    #[error("persist error: {0}")]
    Persist(String),
    #[error("cdc error: {0}")]
    Cdc(String),
}
pub type Result<T> = std::result::Result<T, EngineError>;

fn vector_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("vector.bin")
}

/// pgvector 直查档的过取系数（PG 取 `candidates × 此值` 候选再精确后过滤，抵消损耗）。
const PG_VECTOR_OVER_FETCH: usize = 4;

/// CDC 检查点：派生索引落盘时一并记录的水位（崩溃后从此续传）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
    schema_version: u32,
    /// 已持久化进派生索引的 LSN 水位。
    applied_lsn: u64,
    /// 向量维度（用于检测换模型/换维度需重建）。
    vector_dim: Option<usize>,
    /// 向量后端名（"brute"/"hnsw"）；open 时据此选 loader。空/缺省视为 brute。
    #[serde(default)]
    vector_backend: String,
}

impl Checkpoint {
    fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("checkpoint.json")
    }

    fn load(data_dir: &Path) -> Result<Self> {
        let p = Self::path(data_dir);
        if !p.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(&p).map_err(|e| EngineError::Persist(format!("read checkpoint: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| EngineError::Persist(format!("parse checkpoint: {e}")))
    }

    /// 原子写：tmp → fsync → rename。
    fn save(&self, data_dir: &Path) -> Result<()> {
        let p = Self::path(data_dir);
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let bytes = serde_json::to_vec(self)
            .map_err(|e| EngineError::Persist(format!("ser checkpoint: {e}")))?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| EngineError::Persist(format!("create checkpoint tmp: {e}")))?;
            f.write_all(&bytes)
                .and_then(|_| f.sync_all())
                .map_err(|e| EngineError::Persist(format!("write checkpoint: {e}")))?;
        }
        std::fs::rename(&tmp, &p)
            .map_err(|e| EngineError::Persist(format!("rename checkpoint: {e}")))?;
        Ok(())
    }
}

fn kind_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Heading => "heading",
        ChunkKind::Paragraph => "paragraph",
        ChunkKind::Table => "table",
        ChunkKind::Code => "code",
        ChunkKind::ListItem => "list_item",
        ChunkKind::Image => "image",
        ChunkKind::Audio => "audio",
        ChunkKind::Video => "video",
    }
}

fn vec_meta(collection: &str, c: &Chunk) -> VecMeta {
    VecMeta {
        collection: collection.to_string(),
        doc_id: c.doc_id.clone(),
        chunk_id: c.chunk_id,
        kind: kind_str(c.kind).to_string(),
        modality: c.kind.modality().as_str().to_string(),
        page: c.page,
        section_id: c.section_id,
        heading_path: c.heading_path.clone(),
        tenant: c.tenant.clone(),
        acl: c.acl.clone(),
        bbox: c.bbox,
        time: c.media.as_ref().and_then(|m| m.time),
        media: c.media.clone(),
    }
}

/// 一条检索命中（带引用与分数明细）。
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: GlobalId,
    pub score: f64,
    pub citation: Citation,
    /// BM25 分（keyword/hybrid 路）。
    pub bm25: Option<f32>,
    /// 向量相似分（hybrid 路；vector 后端落地前为 None）。
    pub vector: Option<f64>,
    /// rerank 分（req.rerank 存在时）。
    pub rerank: Option<f64>,
    /// 高亮片段（HTML）；仅 keyword 命中且 req.highlight 时有值。
    pub highlight: Option<String>,
    /// auto-merge 时被并入本代表命中的同 section 兄弟 chunk_id（升序）；未归并为空。
    /// 答案层可据此解析整段的全部引用。
    pub merged_chunk_ids: Vec<u64>,
}

impl SearchHit {
    /// 最终排名的排序键：有 rerank 用 rerank 分，否则用融合分（与 `run` 末端排序一致）。
    fn sort_key(&self) -> f64 {
        self.rerank.unwrap_or(self.score)
    }

    /// 本命中的**深分页游标**（不透明 token）：把它作为下一页的 `search_after` 即从此条之后续取。
    /// 编码 = `排序键 bits(16 hex)` + `:` + `citation_id`（精确 round-trip，确定性）。
    pub fn cursor(&self) -> String {
        format!(
            "{:016x}:{}",
            self.sort_key().to_bits(),
            self.id.to_citation_id()
        )
    }
}

/// 解析深分页游标 → `(排序键, GlobalId)`。前 16 位十六进制是排序键 bits，其后为 citation_id
/// （doc_id 可含 `:`，故只在第 17 位的分隔符处切一次）。非法 → InvalidRequest。
fn parse_cursor(tok: &str) -> Result<(f64, GlobalId)> {
    let bad = || {
        EngineError::Core(fastsearch_core::CoreError::InvalidRequest(
            "invalid search_after cursor".into(),
        ))
    };
    if tok.len() < 18 || tok.as_bytes()[16] != b':' {
        return Err(bad());
    }
    let bits = u64::from_str_radix(&tok[..16], 16).map_err(|_| bad())?;
    let gid = GlobalId::parse(&tok[17..])?;
    Ok((f64::from_bits(bits), gid))
}

/// `resolve_citation` 的结果：如何**安全地**取到这段媒资（已过 ACL）。
#[derive(Debug, Clone)]
pub struct ResolvedAsset {
    pub fetch: AssetFetch,
    pub time: Option<TimeSpan>,
    pub media_type: Option<String>,
}

/// 取媒资字节的方式。
#[derive(Debug, Clone)]
pub enum AssetFetch {
    /// 内联字节（小裁图，字节在 PG `media_bytes`——MM2 接入；当前引擎索引无字节）。
    InlineBytes(Vec<u8>),
    /// 对象存储签名 URL（真签名待对象存储接入；当前回 uri + 过期秒数）。
    SignedUrl { url: String, expires_s: u64 },
    /// 无独立字节：跳转到原文位置（page+bbox），答案层据此深链/高亮。
    DocRender {
        doc_id: String,
        page: u32,
        bbox: BBox,
    },
}

/// 端到端检索引擎。
pub struct Engine {
    text: TextIndex,
    vector: VectorStore,
    reranker: Box<dyn Reranker + Send + Sync>,
    /// 可选嵌入后端：设置后，**CDC 应用路径**（`IndexSink::apply_upsert`）会自动嵌入
    /// chunk 正文并写向量索引（None=仅全文）。详见 `set_embedder`。
    embedder: Option<Box<dyn Embedder + Send + Sync>>,
    /// 可选 **pgvector 直查档（B6）**：设置后，向量召回**绕过引擎侧索引、在 PG 跑 ANN**
    /// （filter/ACL 下推 + iterative scan + 精确后过滤）。仅在 **multi-thread tokio runtime** 下可用
    /// （`run` 内 `block_in_place` 桥接同步检索↔异步 PG 查询）。详见 `set_pg_vector`。
    vector_pg: Option<std::sync::Arc<fastsearch_pg::PgStore>>,
}

impl Engine {
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self> {
        Self::create_in_ram_with(cfg, VectorBackendKind::Brute)
    }

    /// 内存引擎 + 指定向量后端（`Brute` 默认 / `Hnsw` 大规模 opt-in）。
    pub fn create_in_ram_with(cfg: TextIndexConfig, backend: VectorBackendKind) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::create_in_ram(cfg)?,
            vector: VectorStore::new(backend),
            reranker: Box::new(LexicalOverlapReranker),
            embedder: None,
            vector_pg: None,
        })
    }

    pub fn open_or_create(dir: &std::path::Path, cfg: TextIndexConfig) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::open_or_create(dir, cfg)?,
            vector: VectorStore::new(VectorBackendKind::Brute),
            reranker: Box::new(LexicalOverlapReranker),
            embedder: None,
            vector_pg: None,
        })
    }

    /// 打开**数据目录**下的完整派生索引（落盘恢复），向量后端默认 `Brute`。
    pub fn open(data_dir: &Path, cfg: TextIndexConfig) -> Result<(Self, Lsn)> {
        Self::open_with(data_dir, cfg, VectorBackendKind::Brute)
    }

    /// 同 [`open`](Self::open)，但**首启（无检查点）时用 `default_backend`**；已有检查点则沿用
    /// 其记录的后端（不被 default 覆盖）。`<data>/text` + `vector.bin` + `checkpoint.json`。
    pub fn open_with(
        data_dir: &Path,
        cfg: TextIndexConfig,
        default_backend: VectorBackendKind,
    ) -> Result<(Self, Lsn)> {
        let text_dir = data_dir.join("text");
        std::fs::create_dir_all(&text_dir)
            .map_err(|e| EngineError::Persist(format!("create data dir: {e}")))?;
        let text = TextIndex::open_or_create(&text_dir, cfg)?;
        let cp = Checkpoint::load(data_dir)?;
        // 已有检查点 → 沿用其后端（hnsw params 取自快照本身）；无检查点（首启）→ 用传入默认。
        let kind = match cp.vector_backend.as_str() {
            "hnsw" => VectorBackendKind::Hnsw(fastsearch_vector::HnswParams::default()),
            "brute" => VectorBackendKind::Brute,
            _ => default_backend,
        };
        let vector = VectorStore::load(kind, &vector_path(data_dir))
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        // 维度漂移（换了嵌入模型）告警——可见、不静默。
        if let (Some(saved), Some(cur)) = (cp.vector_dim, vector.dim()) {
            if saved != cur {
                eprintln!("warning: checkpoint vector_dim {saved} != loaded {cur}");
            }
        }
        Ok((
            Engine {
                text,
                vector,
                reranker: Box::new(LexicalOverlapReranker),
                embedder: None,
                vector_pg: None,
            },
            Lsn(cp.applied_lsn),
        ))
    }

    /// 持久化派生索引 + 检查点（先落盘、后由调用方推进 slot——崩溃安全的前提）：
    /// `text.commit()` → 向量原子落盘 → `checkpoint.json` 原子写入 `applied_lsn`。
    pub fn persist(&mut self, data_dir: &Path, applied_lsn: Lsn) -> Result<()> {
        self.text.commit()?;
        self.vector
            .save(&vector_path(data_dir))
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Checkpoint {
            schema_version: 1,
            applied_lsn: applied_lsn.0,
            vector_dim: self.vector.dim(),
            vector_backend: self.vector.kind_str().to_string(),
        }
        .save(data_dir)?;
        Ok(())
    }

    /// **初始快照 bootstrap**：把已有 PG 行（`(collection, chunk)`）逐条 `apply_upsert`
    /// （经 embedder 嵌入 passage → 写向量索引），再 `persist(data_dir, lsn)`。`lsn` 传 slot
    /// 一致点 → 之后从该 LSN 起增量；幂等保证重叠窗口不产生重复（见
    /// [计划](../../docs/plans/2026-06-25-初始快照-bootstrap.md)）。返回导入条数。
    pub fn bootstrap_snapshot(
        &mut self,
        rows: &[(String, Chunk)],
        data_dir: &Path,
        lsn: Lsn,
    ) -> Result<usize> {
        use fastsearch_sync::IndexSink;
        for (collection, chunk) in rows {
            self.apply_upsert(collection, chunk)
                .map_err(|e| EngineError::Cdc(format!("bootstrap apply: {e}")))?;
        }
        self.persist(data_dir, lsn)?;
        Ok(rows.len())
    }

    /// **单集合原地重建**（坏索引/索引损坏 → 从真源 PG 重灌）：清空派生 text+vector 索引，
    /// 用传入的 `rows`（PG 全表/单集合快照，真源）经 `apply_upsert` 重灌（含嵌入），统一
    /// `commit` 成一次可见切换。**派生可重建**不变量的运维出口；不触 PG，调用方负责 fetch。
    ///
    /// 换分词器属"换 schema"，走另一条路（用新 `TextIndexConfig` 新建 Engine + `bootstrap_snapshot`）——
    /// 本方法保持同 schema。返回重灌条数。
    pub fn rebuild_from(&mut self, rows: &[(String, Chunk)]) -> Result<usize> {
        use fastsearch_sync::IndexSink;
        self.text.clear()?;
        self.vector.clear();
        for (collection, chunk) in rows {
            self.apply_upsert(collection, chunk)
                .map_err(|e| EngineError::Cdc(format!("rebuild apply: {e}")))?;
        }
        self.text.commit()?;
        Ok(rows.len())
    }

    /// **崩溃安全地**消费一批 CDC 变更并落地（生产 CDC 主循环的一拍）：
    /// `peek`（不推进 slot）→ 幂等应用全部（`apply_upsert` 含嵌入）→ `persist`（索引 +
    /// 检查点=slot 高水位）→ **落盘成功后才** `advance_slot`。返回应用条数。
    ///
    /// **不靠 LSN 水位跳过**：`pg_logical_slot_peek` 的逐行 lsn 对一个事务的 Begin/Insert
    /// 报的是事务起点（首事务等于 slot 一致点），用它做水位会误跳首批。正确性靠：① slot
    /// 在 `advance` 前不重投；② 应用按 `GlobalId` upsert/delete **幂等**——崩溃重投同结果。
    /// 故每拍用 `Applier::new(Lsn(0))` 应用全部 peek 到的变更。
    pub async fn consume_once(
        &mut self,
        cfg: &ReplicationConfig,
        data_dir: &Path,
    ) -> Result<usize> {
        let (events, slot_lsn) = peek_with_lsn(cfg)
            .await
            .map_err(|e| EngineError::Cdc(format!("peek: {e}")))?;
        if events.is_empty() {
            // 仅非数据消息推进了 WAL：把 slot 推到已查看最高位，避免空转重读。
            if slot_lsn > Lsn(0) {
                advance_slot(cfg, slot_lsn)
                    .await
                    .map_err(|e| EngineError::Cdc(format!("advance: {e}")))?;
            }
            return Ok(0);
        }
        let mut applier = Applier::new(Lsn(0)); // 不跳过：应用全部（见上）
        let applied = applier
            .apply_batch(self, &events)
            .map_err(|e| EngineError::Cdc(format!("apply: {e}")))?;
        // 先落盘（索引 + 检查点=slot 高水位，含 Commit），后推进 slot —— 崩溃安全铁律。
        self.persist(data_dir, slot_lsn)?;
        advance_slot(cfg, slot_lsn)
            .await
            .map_err(|e| EngineError::Cdc(format!("advance: {e}")))?;
        Ok(applied)
    }

    /// 替换 reranker（接入真 cross-encoder 时用）。
    pub fn set_reranker(&mut self, reranker: Box<dyn Reranker + Send + Sync>) {
        self.reranker = reranker;
    }

    /// 设置嵌入后端：开启后 **CDC 落地（`apply_upsert`）自动嵌入 chunk 正文 → 写向量索引**，
    /// 使"PG 写 → 复制 → 解码 → 嵌入 → 派生 BM25+向量"主循环完整成立。None=仅全文。
    pub fn set_embedder(&mut self, embedder: Box<dyn Embedder + Send + Sync>) {
        self.embedder = Some(embedder);
    }

    /// 开启 **pgvector 直查档（B6）**：向量召回改在 PG 跑 ANN（见字段 `vector_pg`）。
    /// **要求 multi-thread tokio runtime**（检索在 `block_in_place` 里 `block_on` PG 异步查询）。
    /// 仅影响向量召回；keyword 仍走引擎 Tantivy。embedding 需已在 PG（外部写入或写穿）。
    pub fn set_pg_vector(&mut self, store: std::sync::Arc<fastsearch_pg::PgStore>) {
        self.vector_pg = Some(store);
    }

    /// 灌入一个 chunk（仅全文，不提交）。
    pub fn ingest(&mut self, collection: &str, chunk: &Chunk) -> Result<()> {
        self.text.upsert(collection, chunk)?;
        Ok(())
    }

    /// 灌入一个 chunk + 其向量（全文 + 向量索引，不提交）。
    pub fn ingest_vector(
        &mut self,
        collection: &str,
        chunk: &Chunk,
        vector: Vec<f32>,
    ) -> Result<()> {
        self.text.upsert(collection, chunk)?;
        self.vector
            .upsert(
                chunk.global_id(collection),
                vector,
                vec_meta(collection, chunk),
            )
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    /// 删除一个 chunk（全文 + 向量，不提交）。
    pub fn remove(&mut self, gid: &GlobalId) -> Result<()> {
        self.text.delete_by_global_id(gid)?;
        self.vector
            .delete(gid)
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    /// 删除某 doc 全部 chunk（全文 + 向量，不提交）。
    pub fn remove_doc(&mut self, collection: &str, doc_id: &str) -> Result<()> {
        self.text.delete_by_doc(collection, doc_id)?;
        self.vector
            .delete_doc(collection, doc_id)
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.text.commit()?;
        Ok(())
    }

    /// 排序管线检索：ACL 强制注入（text/vector 各自落实，不可绕过）→ keyword∥semantic
    /// 召回 → core::fuse 融合 → 组装带引用命中。
    ///
    /// - mode=Keyword：仅全文。mode=Vector：仅向量（需 `req.vector`）。
    /// - mode=Hybrid：两路并行 + 融合（无 `req.vector` 时退化为全文）。
    /// - `fuse` 自带"一路空退化"，故统一调用。
    pub fn search(&self, req: &SearchRequest, acl: Option<&AclFilter>) -> Result<Vec<SearchHit>> {
        Ok(self.run(req, acl)?.0)
    }

    /// 同 [`Engine::search`]，外加按 `req.facets` 计算分面计数（当前支持 `kind`/`doc_id`）。
    pub fn search_with_facets(
        &self,
        req: &SearchRequest,
        acl: Option<&AclFilter>,
    ) -> Result<(Vec<SearchHit>, Facets)> {
        self.run(req, acl)
    }

    /// 由 `citation_id` + ACL 解析"如何安全取到这段媒资"。**ACL 在此强制**：解析出的
    /// chunk 不可见 → 返回 `None`（等同 404，不暴露存在性）。无媒资/Inline 字节不在引擎
    /// （在 PG `media_bytes`，MM2）→ `None`。详见多模态计划 §6。
    pub fn resolve_citation(
        &self,
        cid: &str,
        acl: Option<&AclFilter>,
    ) -> Result<Option<ResolvedAsset>> {
        let gid = GlobalId::parse(cid)?;
        let row = match self.text.stored_row_by_gid(&gid)? {
            Some(r) => r,
            None => return Ok(None),
        };
        // ACL 强制注入（不可绕过）：不可见 → None，等同 404。
        if let Some(a) = acl {
            if !a.visible(&row) {
                return Ok(None);
            }
        }
        let media = match row.media {
            Some(m) => m,
            None => return Ok(None), // 该 chunk 无媒资
        };
        let fetch = match media.asset {
            AssetPointer::DocRegion { page, bbox } => AssetFetch::DocRender {
                doc_id: row.doc_id,
                page,
                bbox,
            },
            AssetPointer::Object { uri } => AssetFetch::SignedUrl {
                url: uri,
                expires_s: 300,
            },
            // Inline 字节在 PG media_bytes（MM2 接入）；引擎索引不持字节。
            AssetPointer::Inline => return Ok(None),
        };
        Ok(Some(ResolvedAsset {
            fetch,
            time: media.time,
            media_type: media.media_type,
        }))
    }

    /// more_like_this：以种子 chunk 的正文反查相似命中（keyword 模式），排除种子自身。
    /// 种子不存在 → 返回空。ACL 照常强制（不可绕过）。
    pub fn more_like_this(
        &self,
        gid: &GlobalId,
        top_k: usize,
        acl: Option<&AclFilter>,
    ) -> Result<Vec<SearchHit>> {
        let seed_text = match self.text.stored_text(gid)? {
            Some(t) => t,
            None => return Ok(vec![]),
        };
        let query = mlt_query(&seed_text);
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let req = SearchRequest {
            query,
            mode: SearchMode::Keyword,
            top_k: top_k + 1, // 多取一条，扣掉种子自身
            candidates: (top_k + 1).max(SearchRequest::default().candidates),
            ..Default::default()
        };
        let mut hits = self.run(&req, acl)?.0;
        hits.retain(|h| &h.id != gid);
        hits.truncate(top_k);
        Ok(hits)
    }

    fn run(
        &self,
        req: &SearchRequest,
        acl: Option<&AclFilter>,
    ) -> Result<(Vec<SearchHit>, Facets)> {
        req.validate()?;
        let candidates = req.candidates.max(req.top_k);

        let want_kw = matches!(req.mode, SearchMode::Keyword | SearchMode::Hybrid);
        let want_vec =
            matches!(req.mode, SearchMode::Vector | SearchMode::Hybrid) && req.vector.is_some();

        // keyword 召回
        let kw_hits: Vec<TextHit> = if want_kw {
            self.text.search(
                &req.query,
                req.filter.as_ref(),
                acl,
                candidates,
                req.highlight,
            )?
        } else {
            vec![]
        };
        // semantic 召回（filter-aware，真预过滤）。pgvector 直查档（B6）时绕引擎索引、在 PG 跑 ANN，
        // 并带回 citation（page/bbox，PG SELECT 出来）；否则走引擎侧向量后端。
        let mut pg_citations: HashMap<GlobalId, Citation> = HashMap::new();
        let vec_scored: Vec<Scored> = if !want_vec {
            vec![]
        } else if let Some(pg) = &self.vector_pg {
            let qv = req.vector.as_ref().unwrap();
            // 同步检索↔异步 PG：block_in_place 桥接（要求 multi-thread runtime）。
            let pairs = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(pg.vector_search(
                    qv,
                    candidates,
                    PG_VECTOR_OVER_FETCH,
                    acl,
                    req.filter.as_ref(),
                ))
            })
            .map_err(|e| EngineError::Vector(e.to_string()))?;
            pairs
                .into_iter()
                .map(|(s, c)| {
                    pg_citations.insert(s.id.clone(), c);
                    s
                })
                .collect()
        } else {
            self.vector
                .search(
                    req.vector.as_ref().unwrap(),
                    candidates,
                    req.filter.as_ref(),
                    acl,
                )
                .map_err(|e| EngineError::Vector(e.to_string()))?
        };

        // 分面：在（keyword）候选集上按字段计数（kind / doc_id）。
        let facets = compute_facets(&req.facets, &kw_hits);

        // 查找表：引用 / 各路分
        let mut kw_score: HashMap<GlobalId, f32> = HashMap::new();
        let mut citation: HashMap<GlobalId, Citation> = HashMap::new();
        let mut highlight: HashMap<GlobalId, String> = HashMap::new();
        let mut text_map: HashMap<GlobalId, String> = HashMap::new();
        for h in &kw_hits {
            kw_score.insert(h.id.clone(), h.score);
            citation.insert(h.id.clone(), h.citation.clone());
            text_map.insert(h.id.clone(), h.text.clone());
            if let Some(hl) = &h.highlight {
                highlight.insert(h.id.clone(), hl.clone());
            }
        }
        let mut vec_score: HashMap<GlobalId, f64> = HashMap::new();
        for s in &vec_scored {
            vec_score.insert(s.id.clone(), s.score);
            citation.entry(s.id.clone()).or_insert_with(|| {
                // pgvector 直查档：用 PG 带回的真实引用；否则引擎侧向量后端；都无则退化占位。
                pg_citations
                    .get(&s.id)
                    .cloned()
                    .or_else(|| self.vector.citation(&s.id))
                    .unwrap_or_else(|| Citation {
                        collection: s.id.collection.clone(),
                        doc_id: s.id.doc_id.clone(),
                        chunk_id: s.id.chunk_id,
                        page: 0,
                        bbox: fastsearch_core::BBox {
                            x0: 0.0,
                            y0: 0.0,
                            x1: 0.0,
                            y1: 0.0,
                        },
                        heading_path: vec![],
                        section_id: 0,
                        time: None,
                        media: None,
                    })
            });
        }

        // 融合（一路空自动退化）
        let kw_list: Vec<Scored> = kw_hits
            .iter()
            .map(|h| Scored {
                id: h.id.clone(),
                score: h.score as f64,
            })
            .collect();
        let fused = fuse(&kw_list, &vec_scored, &req.fusion);

        let mut hits: Vec<SearchHit> = fused
            .into_iter()
            .filter_map(|s| {
                citation.get(&s.id).map(|c| SearchHit {
                    id: s.id.clone(),
                    score: s.score,
                    citation: c.clone(),
                    bm25: kw_score.get(&s.id).copied(),
                    vector: vec_score.get(&s.id).copied(),
                    rerank: None,
                    highlight: highlight.get(&s.id).cloned(),
                    merged_chunk_ids: Vec::new(),
                })
            })
            .collect();

        // auto-merging：同 (doc_id, section_id) 的多个命中片段归并为最高排名的代表，
        // 其余兄弟 chunk_id 记入代表的 merged_chunk_ids 后移除（保序、确定性）。
        // 仅对 section_id != 0（真段）归并；section_id==0 视为"无段"不并。
        if req.auto_merge {
            hits = auto_merge(hits);
        }

        // rerank：宽召回后重排（req.rerank 存在时）。对候选文本打分、按 rerank 分降序、
        // 同分按 gid，再截 top_k。rerank 分写入命中（透明）；原 bm25/vector/fused 保留。
        if req.rerank.is_some() {
            let texts: Vec<String> = hits
                .iter()
                .map(|h| text_map.get(&h.id).cloned().unwrap_or_default())
                .collect();
            let scores = self
                .reranker
                .rerank(&req.query, &texts)
                .map_err(|e| EngineError::Rerank(e.to_string()))?;
            for (h, sc) in hits.iter_mut().zip(scores) {
                h.rerank = Some(sc);
            }
            hits.sort_by(|a, b| {
                b.rerank
                    .partial_cmp(&a.rerank)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        // 分组折叠：按最终排名，每组（doc_id/section_id）至多保留 max_per_group 条。
        if let Some(c) = &req.collapse {
            hits = collapse_groups(hits, &c.field, c.max_per_group);
        }
        // 深分页：只保留最终排名中**严格在游标之后**的命中（与 (排序键 desc, gid asc) 一致：
        // 分更低，或同分而 gid 更大）。深度受 `candidates` 候选窗口约束——游标落在窗口外则
        // 该页可能短/空，加大 `candidates` 可加深（标准 search_after 取舍，诚实记账）。
        if let Some(tok) = &req.search_after {
            let (ck, cgid) = parse_cursor(tok)?;
            hits.retain(|h| {
                let k = h.sort_key();
                k < ck || (k == ck && h.id > cgid)
            });
        }
        hits.truncate(req.top_k);
        Ok((hits, facets))
    }
}

/// 分面结果：字段 → [(值, 计数)]（按计数降序、值升序，确定性）。
pub type Facets = std::collections::BTreeMap<String, Vec<(String, u64)>>;

fn compute_facets(fields: &[String], hits: &[TextHit]) -> Facets {
    let mut out = Facets::new();
    for field in fields {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for h in hits {
            let val = match field.as_str() {
                "kind" => Some(h.kind.clone()),
                "modality" => Some(
                    fastsearch_core::Modality::of_kind_str(&h.kind)
                        .as_str()
                        .to_string(),
                ),
                "doc_id" => Some(h.id.doc_id.clone()),
                _ => None, // 支持 kind / modality / doc_id
            };
            if let Some(v) = val {
                *counts.entry(v).or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            continue;
        }
        let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.insert(field.clone(), pairs);
    }
    out
}

/// 把同 `(doc_id, section_id)`（`section_id != 0`）的多个命中片段归并为组内最高排名的
/// 代表命中：被并入的兄弟 chunk_id 记入代表的 `merged_chunk_ids`（升序去重）后移除。
/// 输入须已按最终排名排序（代表 = 组内首个出现者）；输出保序、确定性。
fn auto_merge(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut out: Vec<SearchHit> = Vec::with_capacity(hits.len());
    // (doc_id, section_id) → out 中代表命中的下标。
    let mut rep: HashMap<(String, u64), usize> = HashMap::new();
    for h in hits {
        let sec = h.citation.section_id;
        if sec == 0 {
            out.push(h); // 无段，不参与归并
            continue;
        }
        let key = (h.id.doc_id.clone(), sec);
        match rep.get(&key) {
            Some(&idx) => {
                // 已有代表：把本命中并入（记录 chunk_id），丢弃本命中。
                let cid = h.id.chunk_id;
                let merged = &mut out[idx].merged_chunk_ids;
                if let Err(pos) = merged.binary_search(&cid) {
                    merged.insert(pos, cid);
                }
            }
            None => {
                rep.insert(key, out.len());
                out.push(h);
            }
        }
    }
    out
}

/// 把种子正文净化成 keyword 查询：剔除 Tantivy 查询元字符（保留字母/数字/CJK/空白），
/// 取前 `MLT_TERMS` 个词。避免长文本/特殊字符破坏 QueryParser。
const MLT_TERMS: usize = 20;
fn mlt_query(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned
        .split_whitespace()
        .take(MLT_TERMS)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 按最终排名折叠分组：每个分组键至多保留 `max_per_group` 条（高分者优先）。
/// `field` 支持 `doc_id` / `section_id`；其他值视为不折叠（原样返回）。保序、确定性。
fn collapse_groups(hits: Vec<SearchHit>, field: &str, max_per_group: usize) -> Vec<SearchHit> {
    if field != "doc_id" && field != "section_id" {
        return hits; // 未知折叠字段：不折叠
    }
    let mut out: Vec<SearchHit> = Vec::with_capacity(hits.len());
    let mut counts: HashMap<String, usize> = HashMap::new();
    for h in hits {
        let key = match field {
            "doc_id" => h.id.doc_id.clone(),
            _ => h.citation.section_id.to_string(),
        };
        let n = counts.entry(key).or_insert(0);
        if *n < max_per_group {
            *n += 1;
            out.push(h);
        }
    }
    out
}

/// CDC 落地：sync 的变更应用到 text 索引。放在 engine 而非 text，避免 text 反依赖 sync。
impl fastsearch_sync::IndexSink for Engine {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()> {
        self.text.upsert(collection, chunk)?;
        // 配了嵌入后端则同步写向量索引（CDC 主循环：复制→解码→嵌入→派生向量）。
        // **MM5 modality 路由（M0）**：当前只有文本嵌入器，按"可检索文本表示"(`chunk.text`，
        // 含 caption/转录) 嵌入。**无文本的媒资 chunk（如无 caption 的图，`text=""`）跳过向量**——
        // 否则空串嵌成退化向量（HashEmbedder 仅前缀 token → 所有无文本媒资塌成同一向量；真嵌入器
        // 给"空文档"向量），污染 ANN。这些 chunk 仍在 BM25 + modality fast field（可按模态召回），
        // 视觉向量待 M1 图像嵌入路由接入。
        if let Some(emb) = &self.embedder {
            if !chunk.text.trim().is_empty() {
                let v = emb
                    .embed(std::slice::from_ref(&chunk.text), EmbedKind::Passage)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
                self.vector
                    .upsert(chunk.global_id(collection), v, vec_meta(collection, chunk))
                    .map_err(|e| anyhow::anyhow!("vector upsert: {e}"))?;
            } else {
                // 幂等：覆盖更新时若旧版本有向量、新版本文本空了，删除旧向量避免残留。
                self.vector.delete(&chunk.global_id(collection))?;
            }
        }
        Ok(())
    }
    fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        self.text.delete_by_global_id(gid)?;
        self.vector.delete(gid)?;
        Ok(())
    }
    fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        self.text.delete_by_doc(collection, doc_id)?;
        self.vector.delete_doc(collection, doc_id)?;
        Ok(())
    }
    fn commit(&mut self) -> anyhow::Result<()> {
        self.text.commit()?;
        Ok(())
    }
}

/// 相关性评测闭环（F39）：把 [`GoldenSet`](fastsearch_eval::GoldenSet) 语料灌入引擎、
/// 对每个查询跑**真实检索**、用判定算指标。eval crate 只管指标与门禁、不跑检索（守住
/// 分层）；"跑检索"这步落在 engine 这里。
///
/// CI 回归门禁的用法：固定 golden 集 + 提交的 baseline [`Metrics`](fastsearch_eval::Metrics)
/// → `run()` 算当前指标 → [`assert_no_regression`](fastsearch_eval::assert_no_regression)。
pub mod golden {
    use crate::{Engine, Result};
    use fastsearch_core::{SearchMode, SearchRequest};
    use fastsearch_eval::{evaluate, GoldenSet, Metrics, RankedResults};
    use fastsearch_text::TextIndexConfig;

    /// 把 golden 语料灌入一个内存引擎，对每个查询跑 `mode` 检索取 top-`k`，算指标均值。
    ///
    /// - 确定性：`mode=Keyword` 不需要嵌入，CI 可零重依赖跑（推荐做门禁）。
    /// - `cfg` 决定分词等索引参数（中文 golden 用 `TokenizerKind::Jieba`）。
    /// - 判定 key 非法 citation_id → 返回 [`EngineError::Core`](crate::EngineError::Core)。
    pub fn run(
        set: &GoldenSet,
        cfg: TextIndexConfig,
        mode: SearchMode,
        k: usize,
    ) -> Result<Metrics> {
        let mut engine = Engine::create_in_ram(cfg)?;
        for c in &set.corpus {
            engine.ingest(&set.collection, c)?;
        }
        engine.commit()?;

        let judg = set.judgments()?;
        let mut results = RankedResults::new();
        for q in &set.queries {
            let req = SearchRequest {
                query: q.query.clone(),
                mode,
                top_k: k,
                // candidates 必须 >= top_k（见 SearchRequest::validate）。
                candidates: k.max(SearchRequest::default().candidates),
                ..Default::default()
            };
            let hits = engine.search(&req, None)?;
            results.set(q.query.clone(), hits.into_iter().map(|h| h.id).collect());
        }
        Ok(evaluate(&results, &judg, k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, ChunkKind, FieldValue, Filter};
    use fastsearch_sync::{Applier, Change, ChangeEvent, Lsn};

    fn chunk(doc: &str, id: u64, kind: ChunkKind, text: &str, page: u32) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind,
            text: text.into(),
            page,
            bbox: BBox {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            },
            heading_path: vec!["第3章".into()],
            section_id: 7,
            char_len: text.chars().count() as u32,
            media: None,
            tenant: None,
            acl: vec!["public".into()],
        }
    }

    fn engine() -> Engine {
        Engine::create_in_ram(TextIndexConfig::default()).unwrap()
    }

    fn req(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            ..Default::default()
        }
    }

    fn chunk_sec(doc: &str, id: u64, text: &str, section_id: u64) -> Chunk {
        Chunk {
            section_id,
            ..chunk(doc, id, ChunkKind::Paragraph, text, 1)
        }
    }

    #[test]
    fn hnsw_backend_end_to_end() {
        use fastsearch_vector::HnswParams;
        let mut e = Engine::create_in_ram_with(
            TextIndexConfig::default(),
            VectorBackendKind::Hnsw(HnswParams::default()),
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "x", 1),
            vec![1.0, 0.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "y", 1),
            vec![0.0, 1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 3, ChunkKind::Paragraph, "z", 1),
            vec![0.0, 0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(vec![0.9, 0.1, 0.0]),
            ..Default::default()
        };
        let hits = e.search(&r, None).unwrap();
        assert!(!hits.is_empty());
        // 最近 [1,0,0] → chunk 1 居首（HNSW 后端经引擎端到端可用）
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    /// B6 直查档接入引擎（需 DATABASE_URL + multi-thread runtime）：向量召回在 PG 跑 ANN，
    /// 引擎拿到带真实 page/bbox 的引用并完成排序。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pgvector_backend_via_engine() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip pgvector_backend_via_engine: DATABASE_URL not set");
            return;
        };
        use fastsearch_pg::{PgConfig, PgStore, VectorType};
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_eng_vec_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let mut store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");
        // 3 个 chunk（page 各异），写正交向量。
        let mk = |id: u64, page: u32| Chunk {
            page,
            ..chunk("d.pdf", id, ChunkKind::Paragraph, &format!("c{id}"), 1)
        };
        let chunks = vec![mk(1, 11), mk(2, 22), mk(3, 33)];
        store
            .upsert_doc("kb", "d.pdf", &chunks)
            .await
            .expect("upsert");
        for id in 1..=3u64 {
            let mut e = vec![0.0f32; 4];
            e[(id - 1) as usize] = 1.0;
            store
                .set_embedding("kb", "d.pdf", id, &e)
                .await
                .expect("emb");
        }

        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_pg_vector(std::sync::Arc::new(store));
        // 查最接近 [0,1,0,0] = chunk 2（page 22）。
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(vec![0.1, 0.9, 0.0, 0.0]),
            ..Default::default()
        };
        let hits = engine.search(&r, None).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].id.chunk_id, 2, "PG ANN 最近邻应为 chunk 2");
        assert_eq!(hits[0].citation.page, 22, "引用 page 应来自 PG（非退化 0）");
        assert!(hits[0].vector.is_some(), "应有向量分");
    }

    #[test]
    fn search_after_tiles_full_ranking() {
        let mut e = engine();
        // 混合：一条高 tf（分更高）+ 多条同分（靠 gid tie-break），覆盖游标的"同分"分支。
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "data data", 1),
        )
        .unwrap();
        for (doc, id) in [
            ("a.pdf", 2),
            ("a.pdf", 3),
            ("a.pdf", 4),
            ("b.pdf", 1),
            ("b.pdf", 2),
        ] {
            e.ingest("kb", &chunk(doc, id, ChunkKind::Paragraph, "data", 1))
                .unwrap();
        }
        e.commit().unwrap();

        let full = e.search(&req("data"), None).unwrap();
        assert_eq!(full.len(), 6);
        let full_ids: Vec<_> = full.iter().map(|h| h.id.clone()).collect();

        // 逐页 size=2 翻完，应无缝平铺完整排名（无重叠、无遗漏）。
        let mut paged: Vec<GlobalId> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let r = SearchRequest {
                query: "data".into(),
                top_k: 2,
                search_after: cursor.clone(),
                ..Default::default()
            };
            let page = e.search(&r, None).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 2);
            cursor = Some(page.last().unwrap().cursor());
            paged.extend(page.iter().map(|h| h.id.clone()));
        }
        assert_eq!(paged, full_ids, "分页平铺应等于完整排名");

        // 用第 3 条的游标取下一页，应正好接续完整排名的第 4 条起。
        let after3 = SearchRequest {
            query: "data".into(),
            top_k: 10,
            search_after: Some(full[2].cursor()),
            ..Default::default()
        };
        let tail: Vec<_> = e
            .search(&after3, None)
            .unwrap()
            .iter()
            .map(|h| h.id.clone())
            .collect();
        assert_eq!(tail, full_ids[3..]);
    }

    #[test]
    fn search_after_rejects_bad_cursor() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "data", 1))
            .unwrap();
        e.commit().unwrap();
        let r = SearchRequest {
            query: "data".into(),
            search_after: Some("not-a-valid-cursor".into()),
            ..Default::default()
        };
        assert!(e.search(&r, None).is_err());
    }

    #[test]
    fn rebuild_from_truth_replaces_index() {
        let mut e = engine();
        // 旧（可能损坏/过期）索引：含一条将被真源剔除的 chunk。
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "old apple", 1),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "stale banana", 1),
        )
        .unwrap();
        e.commit().unwrap();
        assert_eq!(e.search(&req("banana"), None).unwrap().len(), 1);

        // 从真源重灌：a/2 已不在真源，a/1 内容更新。
        let rows = vec![(
            "kb".to_string(),
            chunk("a.pdf", 1, ChunkKind::Paragraph, "new apple cherry", 1),
        )];
        let n = e.rebuild_from(&rows).unwrap();
        assert_eq!(n, 1);
        // 过期 chunk 已消失；重灌内容可检索。
        assert_eq!(e.search(&req("banana"), None).unwrap().len(), 0);
        let hits = e.search(&req("cherry"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn auto_merge_collapses_same_section() {
        let mut e = engine();
        // 同 doc 同 section 3 个片段；另一 section 1 个；另一 doc 同 section 号 1 个。
        e.ingest("kb", &chunk_sec("a.pdf", 1, "data alpha", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 2, "data beta", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 3, "data gamma", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 4, "data delta", 11))
            .unwrap();
        e.ingest("kb", &chunk_sec("b.pdf", 9, "data epsilon", 10))
            .unwrap();
        e.commit().unwrap();

        // 不归并：5 条
        let plain = e.search(&req("data"), None).unwrap();
        assert_eq!(plain.len(), 5);

        // 归并：a.pdf§10 三条→1 代表，a.pdf§11 一条，b.pdf§10 一条 → 共 3 条
        let mut r = req("data");
        r.auto_merge = true;
        let merged = e.search(&r, None).unwrap();
        assert_eq!(merged.len(), 3);
        // 找 a.pdf§10 的代表：应携带另外两个兄弟 chunk_id（升序）
        let rep = merged
            .iter()
            .find(|h| h.id.doc_id == "a.pdf" && h.citation.section_id == 10)
            .unwrap();
        let mut others: Vec<u64> = [1u64, 2, 3]
            .into_iter()
            .filter(|c| *c != rep.id.chunk_id)
            .collect();
        others.sort_unstable();
        assert_eq!(rep.merged_chunk_ids, others);
        // 其余两条不携带归并
        for h in &merged {
            if !(h.id.doc_id == "a.pdf" && h.citation.section_id == 10) {
                assert!(h.merged_chunk_ids.is_empty());
            }
        }
    }

    #[test]
    fn more_like_this_finds_similar_excludes_seed() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                1,
                ChunkKind::Paragraph,
                "machine learning models",
                1,
            ),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                2,
                ChunkKind::Paragraph,
                "learning models tuning",
                2,
            ),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                3,
                ChunkKind::Paragraph,
                "cooking recipes dinner",
                3,
            ),
        )
        .unwrap();
        e.commit().unwrap();
        let seed = GlobalId {
            collection: "kb".into(),
            doc_id: "a.pdf".into(),
            chunk_id: 1,
        };
        let hits = e.more_like_this(&seed, 10, None).unwrap();
        // 不含种子自身
        assert!(hits.iter().all(|h| h.id.chunk_id != 1));
        // chunk 2（共享 learning/models）应命中，chunk 3（无重叠）不该在最前
        assert!(hits.iter().any(|h| h.id.chunk_id == 2));
        assert_eq!(hits[0].id.chunk_id, 2);
        // 种子不存在 → 空
        let missing = GlobalId {
            collection: "kb".into(),
            doc_id: "a.pdf".into(),
            chunk_id: 999,
        };
        assert!(e.more_like_this(&missing, 10, None).unwrap().is_empty());
    }

    #[test]
    fn modality_filter_two_end() {
        let mut e = engine();
        // 同文本不同 kind：image / audio / paragraph(text)
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Image, "data here", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Audio, "data here", 2))
            .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 3, ChunkKind::Paragraph, "data here", 3),
        )
        .unwrap();
        e.commit().unwrap();
        // keyword 路 + modality=image → 仅 chunk 1（text 侧 modality 由 kind 派生后过滤）
        let mut r = req("data");
        r.filter = Some(Filter::Eq(
            "modality".into(),
            FieldValue::Str("image".into()),
        ));
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        // modality=text → 仅 paragraph(chunk 3)
        let mut r2 = req("data");
        r2.filter = Some(Filter::Eq(
            "modality".into(),
            FieldValue::Str("text".into()),
        ));
        let h2 = e.search(&r2, None).unwrap();
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].id.chunk_id, 3);
    }

    #[test]
    fn mm5_textless_media_skips_vector() {
        use fastsearch_embed::{EmbedKind, Embedder, HashEmbedder};
        // MM5（M0 路由）：有文本（caption/转录/正文）→ 嵌入写向量；无文本媒资（text=""）→ 不嵌、
        // 不写向量（否则空串塌成退化向量污染 ANN）。无文本图仍在 BM25 + modality fast field
        // （按模态召回由 fastsearch-text `empty_text_and_modality_fast_field` 覆盖）。
        let emb = HashEmbedder::new(16);
        let mut e = engine();
        e.set_embedder(Box::new(HashEmbedder::new(16)));
        let text_c = chunk(
            "a.pdf",
            1,
            ChunkKind::Paragraph,
            "quarterly gross margin",
            1,
        );
        let img_c = chunk("a.pdf", 2, ChunkKind::Image, "", 2); // 无 caption 的图
                                                                // 经 CDC 落地路径（apply_upsert + 嵌入）灌入。
        e.rebuild_from(&[("kb".to_string(), text_c), ("kb".to_string(), img_c)])
            .unwrap();
        // 向量检索：无文本图没有向量 → 永不出现；修复前它会以"前缀塌缩向量"混入。
        let qv = emb
            .embed(&["quarterly gross margin".into()], EmbedKind::Query)
            .unwrap()
            .remove(0);
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(qv),
            candidates: 10,
            top_k: 10,
            ..Default::default()
        };
        let hits = e.search(&r, None).unwrap();
        assert!(
            hits.iter().any(|h| h.id.chunk_id == 1),
            "有文本 chunk 应被嵌入并向量召回"
        );
        assert!(
            hits.iter().all(|h| h.id.chunk_id != 2),
            "无文本媒资不应进入向量索引（避免退化向量污染 ANN）"
        );
    }

    #[test]
    fn resolve_citation_enforces_acl() {
        use fastsearch_core::{AssetPointer, MediaRef};
        let mut e = engine();
        let mut c = chunk("a.pdf", 1, ChunkKind::Image, "figure caption", 7);
        c.tenant = Some("acme".into());
        c.acl = vec!["team-a".into()];
        c.media = Some(MediaRef {
            asset: AssetPointer::DocRegion {
                page: 7,
                bbox: c.bbox,
            },
            media_type: Some("image/png".into()),
            time: None,
            region: Some(c.bbox),
            caption_source: None,
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        let cid = "kb:a.pdf:1";
        let authorized = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let unauthorized = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-b".into()],
        };
        // 授权 → DocRender（跳原文页/区域）
        let r = e.resolve_citation(cid, Some(&authorized)).unwrap().unwrap();
        assert_eq!(r.media_type.as_deref(), Some("image/png"));
        assert!(matches!(r.fetch, AssetFetch::DocRender { page: 7, .. }));
        // 越权 → None（等同 404，不暴露存在性）
        assert!(e
            .resolve_citation(cid, Some(&unauthorized))
            .unwrap()
            .is_none());
        // 不存在 → None
        assert!(e
            .resolve_citation("kb:a.pdf:999", Some(&authorized))
            .unwrap()
            .is_none());
    }

    #[test]
    fn media_time_surface_on_citation() {
        use fastsearch_core::{AssetPointer, MediaRef, TimeSpan};
        let mut e = engine();
        // 音频 chunk：转录入 text，media 带时间区间 + 对象指针
        let mut c = chunk("a.pdf", 1, ChunkKind::Audio, "meeting transcript notes", 1);
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://b/clip.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: Some(TimeSpan {
                start_ms: 3000,
                end_ms: 8000,
            }),
            region: None,
            caption_source: Some("asr".into()),
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        // keyword 路（转录命中）→ Citation 透出 time/media
        let hits = e.search(&req("transcript"), None).unwrap();
        assert_eq!(hits.len(), 1);
        let cit = &hits[0].citation;
        assert_eq!(cit.time.unwrap().start_ms, 3000);
        let media = cit.media.as_ref().unwrap();
        assert_eq!(media.media_type.as_deref(), Some("audio/mpeg"));
        assert!(matches!(&media.asset, AssetPointer::Object { uri } if uri == "s3://b/clip.mp3"));
    }

    #[test]
    fn modality_facet_counts() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Image, "data x", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Audio, "data y", 2))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 3, ChunkKind::Paragraph, "data z", 3))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.facets = vec!["modality".into()];
        let (_hits, facets) = e.search_with_facets(&r, None).unwrap();
        let m: std::collections::HashMap<_, _> =
            facets.get("modality").unwrap().iter().cloned().collect();
        assert_eq!(m.get("image"), Some(&1));
        assert_eq!(m.get("audio"), Some(&1));
        assert_eq!(m.get("text"), Some(&1));
    }

    #[test]
    fn collapse_caps_hits_per_doc() {
        let mut e = engine();
        // 3 条 a.pdf + 2 条 b.pdf，都含 "data"
        for id in 1..=3 {
            e.ingest(
                "kb",
                &chunk("a.pdf", id, ChunkKind::Paragraph, "data here", 1),
            )
            .unwrap();
        }
        for id in 1..=2 {
            e.ingest(
                "kb",
                &chunk("b.pdf", id, ChunkKind::Paragraph, "data here", 1),
            )
            .unwrap();
        }
        e.commit().unwrap();
        // 不折叠：5 条
        assert_eq!(e.search(&req("data"), None).unwrap().len(), 5);
        // 折叠 doc_id，每组最多 1 → 2 条（a.pdf 1 + b.pdf 1）
        let mut r = req("data");
        r.collapse = Some(fastsearch_core::Collapse {
            field: "doc_id".into(),
            max_per_group: 1,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 2);
        let docs: std::collections::HashSet<_> = hits.iter().map(|h| h.id.doc_id.clone()).collect();
        assert_eq!(docs.len(), 2);
        // 每组最多 2 → 4 条
        r.collapse = Some(fastsearch_core::Collapse {
            field: "doc_id".into(),
            max_per_group: 2,
        });
        assert_eq!(e.search(&r, None).unwrap().len(), 4);
    }

    #[test]
    fn auto_merge_keeps_section_zero_separate() {
        let mut e = engine();
        // section_id=0 视为"无段"，不应被归并到一起。
        e.ingest("kb", &chunk_sec("a.pdf", 1, "data one", 0))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 2, "data two", 0))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.auto_merge = true;
        let merged = e.search(&r, None).unwrap();
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|h| h.merged_chunk_ids.is_empty()));
    }

    #[test]
    fn ingest_search_with_citation() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta", 5),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta gamma", 6),
        )
        .unwrap();
        e.commit().unwrap();
        let hits = e.search(&req("alpha"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        assert_eq!(hits[0].citation.page, 5);
        assert!(hits[0].bm25.is_some());
        assert_eq!(hits[0].vector, None);
        assert!(hits[0].highlight.is_none()); // 默认不高亮
    }

    #[test]
    fn highlight_when_requested() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta gamma", 5),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("beta");
        r.highlight = true;
        let hits = e.search(&r, None).unwrap();
        assert!(hits[0].highlight.as_ref().unwrap().contains("<b>beta</b>"));
    }

    #[test]
    fn end_to_end_via_cdc_sink() {
        // 端到端：CDC ChangeEvent --Applier--> Engine(IndexSink) --索引--> 检索
        let mut e = engine();
        let mut ap = Applier::new(Lsn(0));
        let evs = vec![
            ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 1, ChunkKind::Table, "毛利率下降", 23)),
                },
                lsn: Lsn(1),
            },
            ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 2, ChunkKind::Paragraph, "新产品发布", 3)),
                },
                lsn: Lsn(2),
            },
        ];
        let n = ap.apply_batch(&mut e, &evs).unwrap();
        assert_eq!(n, 2);
        // jieba 默认分词器为 Default；中文用 Default 分词器命中可能受限，故用整词查询
        let hits = e.search(&req("新产品发布"), None).unwrap();
        assert!(hits.iter().any(|h| h.id.chunk_id == 2));
    }

    #[test]
    fn replace_doc_via_cdc() {
        let mut e = engine();
        let mut ap = Applier::new(Lsn(0));
        // 先灌 chunk 1（oldword）
        ap.apply_batch(
            &mut e,
            &[ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 1, ChunkKind::Paragraph, "oldword", 1)),
                },
                lsn: Lsn(1),
            }],
        )
        .unwrap();
        // doc 级替换：DeleteDoc + 新 chunk（newword）
        ap.apply_batch(
            &mut e,
            &[
                ChangeEvent {
                    change: Change::DeleteDoc {
                        collection: "kb".into(),
                        doc_id: "a.pdf".into(),
                    },
                    lsn: Lsn(2),
                },
                ChangeEvent {
                    change: Change::Upsert {
                        collection: "kb".into(),
                        chunk: Box::new(chunk("a.pdf", 9, ChunkKind::Paragraph, "newword", 1)),
                    },
                    lsn: Lsn(3),
                },
            ],
        )
        .unwrap();
        assert_eq!(e.search(&req("oldword"), None).unwrap().len(), 0);
        assert_eq!(e.search(&req("newword"), None).unwrap().len(), 1);
    }

    #[test]
    fn acl_enforced_in_engine() {
        let mut e = engine();
        let mut c1 = chunk("a.pdf", 1, ChunkKind::Paragraph, "secret", 1);
        c1.tenant = Some("acme".into());
        c1.acl = vec!["team-a".into()];
        let mut c2 = chunk("a.pdf", 2, ChunkKind::Paragraph, "secret", 1);
        c2.tenant = Some("acme".into());
        c2.acl = vec!["team-b".into()];
        e.ingest("kb", &c1).unwrap();
        e.ingest("kb", &c2).unwrap();
        e.commit().unwrap();
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let hits = e.search(&req("secret"), Some(&acl)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn filter_applied() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data", 12))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Paragraph, "data", 12))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.filter = Some(Filter::Eq("kind".into(), FieldValue::Str("table".into())));
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn invalid_request_errs() {
        let e = engine();
        let mut r = req("x");
        r.top_k = 0;
        assert!(e.search(&r, None).is_err());
    }

    #[test]
    fn hybrid_falls_back_to_keyword_without_vector() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("alpha");
        r.mode = SearchMode::Hybrid; // 无 req.vector → 退化为全文
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vector, None);
    }

    #[test]
    fn real_hybrid_fuses_keyword_and_vector() {
        let mut e = engine();
        // c1 文本含 "alpha"，向量 [1,0]；c2 文本含 "beta"，向量 [0,1]
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();

        // hybrid：查询词 "alpha" + 查询向量偏向 [0,1]（语义偏 c2）
        let mut r = req("alpha");
        r.mode = SearchMode::Hybrid;
        r.vector = Some(vec![0.0, 1.0]);
        let hits = e.search(&r, None).unwrap();
        // 两路都召回：c1（keyword 命中）+ c2（vector 命中）
        assert_eq!(hits.len(), 2);
        let c1 = hits.iter().find(|h| h.id.chunk_id == 1).unwrap();
        let c2 = hits.iter().find(|h| h.id.chunk_id == 2).unwrap();
        assert!(c1.bm25.is_some()); // c1 有 keyword 分
        assert!(c2.vector.is_some()); // c2 有 vector 分
    }

    #[test]
    fn persist_load_roundtrip_keeps_vectors_and_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TextIndexConfig::default();
        // 首启：空，lsn=0
        let (mut e, lsn0) = Engine::open(dir.path(), cfg).unwrap();
        assert_eq!(lsn0, Lsn(0));
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.persist(dir.path(), Lsn(42)).unwrap();
        drop(e);

        // 重开：检查点续传 + 向量在（无需重嵌）
        let (e2, lsn) = Engine::open(dir.path(), TextIndexConfig::default()).unwrap();
        assert_eq!(lsn, Lsn(42));
        let mut r = req("");
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        let hits = e2.search(&r, None).unwrap();
        assert_eq!(hits[0].id.chunk_id, 1);
        assert!(hits[0].vector.is_some());
        // 文本也在（keyword 路）
        let kw = e2.search(&req("beta"), None).unwrap();
        assert_eq!(kw[0].id.chunk_id, 2);
    }

    #[test]
    fn rerank_reorders_by_overlap() {
        use fastsearch_core::RerankSpec;
        let mut e = engine();
        // 两 chunk 都含 "apple"，但 chunk 2 与 query "apple banana" 词项更重叠
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "apple cherry date", 1),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "apple banana", 2),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("apple banana");
        r.rerank = Some(RerankSpec {
            model: "lexical".into(),
            top_k: 10,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 2);
        // chunk 2（与 query 完全重叠）应被 rerank 提前
        assert_eq!(hits[0].id.chunk_id, 2);
        assert!(hits[0].rerank.unwrap() > hits[1].rerank.unwrap());
    }

    #[test]
    fn facets_count_over_results() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data here", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Table, "data here", 2))
            .unwrap();
        e.ingest(
            "kb",
            &chunk("b.pdf", 3, ChunkKind::Paragraph, "data here", 1),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.facets = vec!["kind".into(), "doc_id".into()];
        let (hits, facets) = e.search_with_facets(&r, None).unwrap();
        assert_eq!(hits.len(), 3);
        // kind: table=2, paragraph=1（降序）
        let kind = &facets["kind"];
        assert_eq!(kind[0], ("table".into(), 2));
        assert_eq!(kind[1], ("paragraph".into(), 1));
        // doc_id: a.pdf=2, b.pdf=1
        let doc = &facets["doc_id"];
        assert_eq!(doc[0], ("a.pdf".into(), 2));
        assert_eq!(doc[1], ("b.pdf".into(), 1));
    }

    #[test]
    fn vector_only_mode() {
        let mut e = engine();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "x", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "y", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req(""); // 纯向量，无关键词
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits[0].id.chunk_id, 1); // 最近向量
        assert!(hits[0].vector.is_some());
        assert!(hits[0].bm25.is_none());
    }
}
