//! 自定义 BM25 重排：让 [`TextIndexConfig`](crate::TextIndexConfig) 的 `k1`/`b` **真生效**。
//!
//! Tantivy 0.26 把 BM25 的 `k1=1.2`/`b=0.75` 写死在 `Bm25Weight`（`const K1/B`），不暴露入口。
//! 本模块在**配置的 `k1`/`b` 偏离默认时**，对 Tantivy 召回的候选集用配置参数**自算 BM25 重排**：
//! 匹配（哪些 doc 命中——boolean/短语/前缀/filter/ACL）仍由 Tantivy 负责，**仅排序**改用自算分。
//! 默认参数(1.2/0.75)下走 Tantivy 原生路径（零改动），故 golden 门禁不受影响。
//!
//! 打分公式与 Tantivy 对齐（多字段加权同 `set_field_boost`）：
//! `score = Σ_term boost · idf · tf·(k1+1) / (tf + k1·(1-b+b·dl/avgdl))`，
//! 其中 `idf = ln(1 + (N-df+0.5)/(df+0.5))`，`dl` = 量化 fieldnorm（与 Tantivy 同源，
//! `FieldNormReader::fieldnorm` 返回解码值），`avgdl = total_num_tokens / N`。
//! 默认参数下自算≈原生（单测 `custom_bm25_near_default_matches_native_order` 守住）。
//!
//! **已知边界（诚实记账）**：① 候选集 = Tantivy 原生 BM25 over-fetch 窗口 → 极端 `k1`/`b` 与候选
//! 窗口交互（同 `search_after` 的窗口约束）；② 短语邻近加成不建模（按构成词求和）；③ 纯前缀
//! （RegexQuery）命中无可计分词 → 不在返回 map 中，调用方对这些 doc 回退原生分。

use std::collections::HashMap;
use tantivy::postings::Postings;
use tantivy::query::{Bm25StatisticsProvider, Query};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocAddress, DocId, DocSet, Searcher, Term, TERMINATED};

/// Tantivy 0.26 硬编码的 BM25 默认参数。
pub(crate) const DEFAULT_K1: f32 = 1.2;
pub(crate) const DEFAULT_B: f32 = 0.75;

/// 配置的 `k1`/`b` 是否偏离 Tantivy 默认——偏离才需自算重排，否则走原生路径零改动。
pub(crate) fn custom_params_active(k1: f32, b: f32) -> bool {
    (k1 - DEFAULT_K1).abs() > f32::EPSILON || (b - DEFAULT_B).abs() > f32::EPSILON
}

/// idf：与 Tantivy `query::bm25::idf` 同式 `ln(1 + (N-df+0.5)/(df+0.5))`。
fn idf(doc_freq: u64, doc_count: u64) -> f32 {
    // 防御：df 理论上 ≤ N，但删除/段合并的极端竞态下取饱和避免下溢。
    let n = doc_count.max(doc_freq);
    let x = ((n - doc_freq) as f32 + 0.5) / (doc_freq as f32 + 0.5);
    (1.0 + x).ln()
}

/// 对候选 doc 用配置 `k1`/`b` 自算 BM25 分。
///
/// 返回 `addr → score`；某 doc 无可计分词（纯前缀命中等）则不在 map 中——调用方回退原生分。
#[allow(clippy::too_many_arguments)]
pub(crate) fn score_candidates(
    searcher: &Searcher,
    query: &dyn Query,
    text_field: Field,
    heading_field: Field,
    heading_boost: f32,
    k1: f32,
    b: f32,
    candidates: &[DocAddress],
) -> tantivy::Result<HashMap<DocAddress, f32>> {
    // 1) 取词：去重，仅保留 text/heading 字段上的项（filter/ACL 项在别的字段，跳过）。
    let mut terms: Vec<Term> = Vec::new();
    query.query_terms(&mut |term: &Term, _need_position: bool| {
        let f = term.field();
        if (f == text_field || f == heading_field) && !terms.contains(term) {
            terms.push(term.clone());
        }
    });
    if terms.is_empty() || candidates.is_empty() {
        return Ok(HashMap::new());
    }

    let n_docs = searcher.num_docs().max(1);
    let avgdl = |field: Field| -> tantivy::Result<f32> {
        let toks = searcher.total_num_tokens(field)?;
        Ok((toks as f32 / n_docs as f32).max(1.0))
    };
    let avgdl_text = avgdl(text_field)?;
    let avgdl_heading = avgdl(heading_field)?;

    // 各 term 的 idf（集合级 doc_freq，跨段）。
    let mut idf_of: HashMap<&Term, f32> = HashMap::with_capacity(terms.len());
    for t in &terms {
        idf_of.insert(t, idf(searcher.doc_freq(t)?, n_docs));
    }

    // 2) 按 segment 分组候选（postings 只能前向 advance，doc 须递增）。
    let mut by_seg: HashMap<u32, Vec<DocId>> = HashMap::new();
    for addr in candidates {
        by_seg
            .entry(addr.segment_ord)
            .or_default()
            .push(addr.doc_id);
    }

    let mut scores: HashMap<DocAddress, f32> = HashMap::new();
    for (seg_ord, mut docs) in by_seg {
        docs.sort_unstable();
        docs.dedup();
        let seg = searcher.segment_reader(seg_ord);
        for (field, boost, avg) in [
            (text_field, 1.0_f32, avgdl_text),
            (heading_field, heading_boost, avgdl_heading),
        ] {
            let inv = seg.inverted_index(field)?;
            let fnr = seg.get_fieldnorms_reader(field)?;
            for t in terms.iter().filter(|t| t.field() == field) {
                let term_idf = idf_of[t];
                let Some(mut postings) = inv.read_postings(t, IndexRecordOption::WithFreqs)? else {
                    continue;
                };
                let mut cur = postings.doc();
                for &doc in &docs {
                    while cur < doc {
                        cur = postings.advance();
                    }
                    if cur == TERMINATED {
                        break;
                    }
                    if cur == doc {
                        let tf = postings.term_freq() as f32;
                        let dl = fnr.fieldnorm(doc) as f32;
                        let norm = k1 * (1.0 - b + b * dl / avg);
                        let contrib = boost * term_idf * (tf * (k1 + 1.0)) / (tf + norm);
                        *scores.entry(DocAddress::new(seg_ord, doc)).or_insert(0.0) += contrib;
                    }
                }
            }
        }
    }
    Ok(scores)
}
