//! # fastsearch-engine
//!
//! 把 core（融合/模型）+ text（全文索引）+ vector（向量后端）+ sync（CDC sink）
//! 整合成端到端引擎：灌入（含经 CDC `IndexSink`）→ 索引 → 排序管线检索 → 带引用命中。
//! 详见 [spec](../../docs/specs/14-engine.md)。
//!
//! 三种检索模式全可用：keyword / vector / **hybrid（keyword∥vector → core::fuse 融合）**。
//! 过滤与 ACL 在两路各自做真预过滤（不可绕过）；分面（kind/doc_id）、高亮、**rerank**
//! （req.rerank 时宽召回后重排）、**auto-merging**（req.auto_merge 时同 section 归并）均已接入。

use fastsearch_core::{
    fuse, AclFilter, Chunk, ChunkKind, Citation, GlobalId, Scored, SearchMode, SearchRequest,
};
use fastsearch_rerank::{LexicalOverlapReranker, Reranker};
use fastsearch_text::{TextHit, TextIndex, TextIndexConfig};
use fastsearch_vector::{MemVectorIndex, VecMeta, VectorBackend};
use std::collections::HashMap;
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
}
pub type Result<T> = std::result::Result<T, EngineError>;

fn kind_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Heading => "heading",
        ChunkKind::Paragraph => "paragraph",
        ChunkKind::Table => "table",
        ChunkKind::Code => "code",
        ChunkKind::ListItem => "list_item",
        ChunkKind::Image => "image",
    }
}

fn vec_meta(collection: &str, c: &Chunk) -> VecMeta {
    VecMeta {
        collection: collection.to_string(),
        doc_id: c.doc_id.clone(),
        chunk_id: c.chunk_id,
        kind: kind_str(c.kind).to_string(),
        page: c.page,
        section_id: c.section_id,
        heading_path: c.heading_path.clone(),
        tenant: c.tenant.clone(),
        acl: c.acl.clone(),
        bbox: c.bbox,
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

/// 端到端检索引擎。
pub struct Engine {
    text: TextIndex,
    vector: MemVectorIndex,
    reranker: Box<dyn Reranker + Send + Sync>,
}

impl Engine {
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::create_in_ram(cfg)?,
            vector: MemVectorIndex::new(),
            reranker: Box::new(LexicalOverlapReranker),
        })
    }

    pub fn open_or_create(dir: &std::path::Path, cfg: TextIndexConfig) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::open_or_create(dir, cfg)?,
            vector: MemVectorIndex::new(),
            reranker: Box::new(LexicalOverlapReranker),
        })
    }

    /// 替换 reranker（接入真 cross-encoder 时用）。
    pub fn set_reranker(&mut self, reranker: Box<dyn Reranker + Send + Sync>) {
        self.reranker = reranker;
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
        // semantic 召回（filter-aware，真预过滤）
        let vec_scored: Vec<Scored> = if want_vec {
            self.vector
                .search(
                    req.vector.as_ref().unwrap(),
                    candidates,
                    req.filter.as_ref(),
                    acl,
                )
                .map_err(|e| EngineError::Vector(e.to_string()))?
        } else {
            vec![]
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
                self.vector.citation(&s.id).unwrap_or_else(|| Citation {
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
                "doc_id" => Some(h.id.doc_id.clone()),
                _ => None, // v1 仅支持 kind/doc_id
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

/// CDC 落地：sync 的变更应用到 text 索引。放在 engine 而非 text，避免 text 反依赖 sync。
impl fastsearch_sync::IndexSink for Engine {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()> {
        self.text.upsert(collection, chunk)?;
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
            image_meta: None,
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
