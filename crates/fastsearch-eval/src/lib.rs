//! # fastsearch-eval
//!
//! 相关性评测体系：nDCG/recall/MRR/precision + CI 回归门禁。这是"完善产品"必备、
//! MVP 最爱砍的部分——没有它无法判断"改动是否让检索更好"。详见
//! [spec](../../docs/specs/18-eval.md)。
//!
//! 纯函数、确定性、不 panic。gid 用 [`GlobalId`]；相关度等级 `grade`（0=不相关，
//! 越大越相关）。

use fastsearch_core::{Chunk, GlobalId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 一个查询的相关性判定：gid → 相关度等级。
pub type Grades = HashMap<GlobalId, u8>;

/// golden 集：query → 判定。
#[derive(Debug, Default, Clone)]
pub struct Judgments {
    map: HashMap<String, Grades>,
}

impl Judgments {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&mut self, query: impl Into<String>, grades: Grades) {
        self.map.insert(query.into(), grades);
    }
    pub fn get(&self, query: &str) -> Option<&Grades> {
        self.map.get(query)
    }
    pub fn queries(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }
}

/// 一批查询的排名结果：query → 按排名的 gid 列表。
#[derive(Debug, Default, Clone)]
pub struct RankedResults {
    map: HashMap<String, Vec<GlobalId>>,
}

impl RankedResults {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&mut self, query: impl Into<String>, ranked: Vec<GlobalId>) {
        self.map.insert(query.into(), ranked);
    }
    pub fn get(&self, query: &str) -> Option<&[GlobalId]> {
        self.map.get(query).map(|v| v.as_slice())
    }
}

/// 各指标的均值（@k）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub ndcg: f64,
    pub recall: f64,
    pub mrr: f64,
    pub precision: f64,
}

fn grade_of(grades: &Grades, gid: &GlobalId) -> u8 {
    grades.get(gid).copied().unwrap_or(0)
}

/// 折扣增益：(2^grade - 1) / log2(rank+2)，rank 从 0。
fn dcg(ranked: &[GlobalId], grades: &Grades, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, gid)| {
            let g = grade_of(grades, gid) as f64;
            (2f64.powf(g) - 1.0) / ((i as f64 + 2.0).log2())
        })
        .sum()
}

/// nDCG@k：DCG / 理想 DCG。IDCG=0（无相关）→ 0。
pub fn ndcg_at_k(ranked: &[GlobalId], grades: &Grades, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let actual = dcg(ranked, grades, k);
    // 理想排序：按 grade 降序。
    let mut ideal: Vec<u8> = grades.values().copied().filter(|g| *g > 0).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, g)| (2f64.powf(*g as f64) - 1.0) / ((i as f64 + 2.0).log2()))
        .sum();
    if idcg <= 0.0 {
        0.0
    } else {
        actual / idcg
    }
}

/// recall@k：top-k 命中的相关数 / 总相关数。
pub fn recall_at_k(ranked: &[GlobalId], grades: &Grades, k: usize) -> f64 {
    let total_relevant = grades.values().filter(|g| **g > 0).count();
    if total_relevant == 0 {
        return 0.0;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|gid| grade_of(grades, gid) > 0)
        .count();
    hit as f64 / total_relevant as f64
}

/// precision@k：top-k 中相关数 / k。
pub fn precision_at_k(ranked: &[GlobalId], grades: &Grades, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|gid| grade_of(grades, gid) > 0)
        .count();
    hit as f64 / k as f64
}

/// MRR：第一个相关命中的 1/rank（rank 从 1）。无→0。
pub fn mrr(ranked: &[GlobalId], grades: &Grades) -> f64 {
    for (i, gid) in ranked.iter().enumerate() {
        if grade_of(grades, gid) > 0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// 对所有有判定的 query 求各指标均值（无判定的 query 跳过）。
pub fn evaluate(results: &RankedResults, judg: &Judgments, k: usize) -> Metrics {
    let mut n = 0usize;
    let (mut nd, mut rc, mut mr, mut pr) = (0.0, 0.0, 0.0, 0.0);
    for query in judg.queries() {
        let grades = match judg.get(query) {
            Some(g) => g,
            None => continue,
        };
        let ranked = results.get(query).unwrap_or(&[]);
        nd += ndcg_at_k(ranked, grades, k);
        rc += recall_at_k(ranked, grades, k);
        mr += mrr(ranked, grades);
        pr += precision_at_k(ranked, grades, k);
        n += 1;
    }
    if n == 0 {
        return Metrics {
            ndcg: 0.0,
            recall: 0.0,
            mrr: 0.0,
            precision: 0.0,
        };
    }
    let nf = n as f64;
    Metrics {
        ndcg: nd / nf,
        recall: rc / nf,
        mrr: mr / nf,
        precision: pr / nf,
    }
}

/// CI 回归门禁：任一指标比 baseline 掉超过 `tol` → Err（附原因）。
pub fn assert_no_regression(baseline: &Metrics, current: &Metrics, tol: f64) -> Result<(), String> {
    let checks = [
        ("ndcg", baseline.ndcg, current.ndcg),
        ("recall", baseline.recall, current.recall),
        ("mrr", baseline.mrr, current.mrr),
        ("precision", baseline.precision, current.precision),
    ];
    for (name, base, cur) in checks {
        if cur + tol < base {
            return Err(format!(
                "regression on {name}: baseline {base:.4} -> current {cur:.4} (tol {tol})"
            ));
        }
    }
    Ok(())
}

/// golden 集（JSON 可加载）：一份固定语料 + 一组带相关性判定的查询。
///
/// 把"语料 + 判定"放在一起，便于 engine 把 `corpus` 灌入索引、对每个 `queries`
/// 跑真实检索、再用 [`judgments`](GoldenSet::judgments) 与判定算指标做 CI 回归门禁
/// （F39 闭环）。eval 自身**不跑检索**（那是 engine 的事，守住分层），只负责
/// 加载 golden、提供指标与门禁。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenSet {
    /// 语料所属集合（参与 `GlobalId` 与 citation_id）。
    pub collection: String,
    /// 待灌入索引的 chunk 语料（复用 [`Chunk`] 的 schema，与 docparse 对齐）。
    pub corpus: Vec<Chunk>,
    /// 带相关性判定的查询。
    pub queries: Vec<GoldenQuery>,
}

/// golden 集里的一条查询及其相关性判定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenQuery {
    /// 查询串（原样喂给引擎）。
    pub query: String,
    /// 相关性判定：`citation_id`（`collection:doc_id:chunk_id`）→ 等级（0=不相关，越大越相关）。
    /// 用完整 citation_id 以规避 doc_id 含 `:` 的歧义。
    pub relevant: HashMap<String, u8>,
}

impl GoldenSet {
    /// 从 JSON 文本加载（语料 + 判定）。schema 错误返回 [`serde_json::Error`]。
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// 把所有查询的判定汇成 [`Judgments`]（query → gid→grade）。
    /// `relevant` 的 key 须为合法 citation_id，否则返回 [`CoreError`](fastsearch_core::CoreError)。
    pub fn judgments(&self) -> fastsearch_core::Result<Judgments> {
        let mut j = Judgments::new();
        for q in &self.queries {
            let mut grades = Grades::new();
            for (cid, grade) in &q.relevant {
                grades.insert(GlobalId::parse(cid)?, *grade);
            }
            j.add(q.query.clone(), grades);
        }
        Ok(j)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gid(n: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: "d".into(),
            chunk_id: n,
        }
    }
    fn grades(pairs: &[(u64, u8)]) -> Grades {
        pairs.iter().map(|(n, g)| (gid(*n), *g)).collect()
    }

    #[test]
    fn ndcg_perfect_and_reverse() {
        // 相关度：g1=3, g2=2, g3=1
        let g = grades(&[(1, 3), (2, 2), (3, 1)]);
        let perfect = vec![gid(1), gid(2), gid(3)];
        assert!((ndcg_at_k(&perfect, &g, 3) - 1.0).abs() < 1e-12);
        // 逆序应 < 1
        let reverse = vec![gid(3), gid(2), gid(1)];
        assert!(ndcg_at_k(&reverse, &g, 3) < 1.0);
        // 无相关 → 0
        assert_eq!(ndcg_at_k(&perfect, &Grades::new(), 3), 0.0);
        assert_eq!(ndcg_at_k(&perfect, &g, 0), 0.0);
    }

    #[test]
    fn ndcg_known_value() {
        // 单个相关项 g=1 在第 2 位：DCG=(2^1-1)/log2(3)=1/1.585=0.6309
        // 理想：g=1 在第 1 位：IDCG=(1)/log2(2)=1 → nDCG=0.6309
        let g = grades(&[(1, 1)]);
        let ranked = vec![gid(9), gid(1)];
        let n = ndcg_at_k(&ranked, &g, 5);
        assert!((n - (1.0 / 3f64.log2())).abs() < 1e-9);
    }

    #[test]
    fn recall_precision() {
        let g = grades(&[(1, 1), (2, 1), (3, 1), (4, 1)]); // 4 个相关
        let ranked = vec![gid(1), gid(9), gid(2), gid(8)]; // top-4 命中 2 个
        assert!((recall_at_k(&ranked, &g, 4) - 0.5).abs() < 1e-12);
        assert!((precision_at_k(&ranked, &g, 4) - 0.5).abs() < 1e-12);
        // k=2：命中 1 个 → recall 1/4, precision 1/2
        assert!((recall_at_k(&ranked, &g, 2) - 0.25).abs() < 1e-12);
        assert!((precision_at_k(&ranked, &g, 2) - 0.5).abs() < 1e-12);
        // 无相关
        assert_eq!(recall_at_k(&ranked, &Grades::new(), 4), 0.0);
    }

    #[test]
    fn mrr_first_relevant_rank() {
        let g = grades(&[(5, 1)]);
        let ranked = vec![gid(1), gid(2), gid(5)]; // 第 3 位
        assert!((mrr(&ranked, &g) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(mrr(&[], &g), 0.0);
        assert_eq!(mrr(&ranked, &Grades::new()), 0.0);
    }

    #[test]
    fn evaluate_averages_and_skips() {
        let mut j = Judgments::new();
        j.add("q1", grades(&[(1, 1)]));
        j.add("q2", grades(&[(2, 1)]));
        let mut r = RankedResults::new();
        r.set("q1", vec![gid(1)]); // 完美
        r.set("q2", vec![gid(9), gid(2)]); // 第 2 位
                                           // q2 无结果? 有。两 query 都算。
        let m = evaluate(&r, &j, 5);
        // recall：q1=1, q2=1 → 均值 1.0
        assert!((m.recall - 1.0).abs() < 1e-12);
        // mrr：q1=1, q2=0.5 → 0.75
        assert!((m.mrr - 0.75).abs() < 1e-12);
    }

    #[test]
    fn evaluate_empty() {
        let m = evaluate(&RankedResults::new(), &Judgments::new(), 5);
        assert_eq!(m.ndcg, 0.0);
        assert_eq!(m.mrr, 0.0);
    }

    #[test]
    fn golden_set_loads_and_builds_judgments() {
        let json = r#"{
            "collection": "kb",
            "corpus": [
                {"doc_id":"d1","chunk_id":0,"kind":"paragraph","text":"hello world",
                 "page":1,"bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":11}
            ],
            "queries": [
                {"query":"hello","relevant":{"kb:d1:0":3,"kb:d1:7":1}}
            ]
        }"#;
        let set = GoldenSet::from_json(json).unwrap();
        assert_eq!(set.collection, "kb");
        assert_eq!(set.corpus.len(), 1);
        assert_eq!(set.corpus[0].doc_id, "d1");
        let j = set.judgments().unwrap();
        let g = j.get("hello").unwrap();
        let gid_d1_0 = GlobalId {
            collection: "kb".into(),
            doc_id: "d1".into(),
            chunk_id: 0,
        };
        assert_eq!(g.get(&gid_d1_0), Some(&3));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn golden_set_rejects_bad_citation() {
        let json = r#"{
            "collection":"kb","corpus":[],
            "queries":[{"query":"x","relevant":{"not-a-citation":1}}]
        }"#;
        let set = GoldenSet::from_json(json).unwrap();
        assert!(set.judgments().is_err());
    }

    #[test]
    fn regression_gate() {
        let base = Metrics {
            ndcg: 0.80,
            recall: 0.90,
            mrr: 0.70,
            precision: 0.60,
        };
        let ok = Metrics {
            ndcg: 0.79,
            recall: 0.90,
            mrr: 0.71,
            precision: 0.60,
        };
        assert!(assert_no_regression(&base, &ok, 0.02).is_ok()); // 掉 0.01 < tol
        let bad = Metrics {
            ndcg: 0.70,
            recall: 0.90,
            mrr: 0.70,
            precision: 0.60,
        };
        let e = assert_no_regression(&base, &bad, 0.02);
        assert!(e.is_err());
        assert!(e.unwrap_err().contains("ndcg"));
    }
}
