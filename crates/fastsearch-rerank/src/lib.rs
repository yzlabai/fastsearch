//! # fastsearch-rerank
//!
//! 排序管线"宽召回 → rerank → top-K"的最后一环。提供 [`Reranker`] trait 与一个
//! **确定性、零依赖**的词项重叠基线 [`LexicalOverlapReranker`]（可测、作 fallback）。
//! 架构决策：RAG 主路径默认不上神经 rerank（答案层 LLM 已做联合打分）；trait 为可选
//! 精度档，服务无-LLM 入口时优先纯 Rust 轻量 LTR。详见 [spec](../../docs/specs/21-rerank.md)。

use std::collections::HashSet;

/// rerank 后端。
pub trait Reranker {
    /// 对每个候选返回相关分（与输入同序）。分越大越相关。
    fn rerank(&self, query: &str, candidates: &[String]) -> anyhow::Result<Vec<f64>>;
}

/// 词项重叠（Jaccard）reranker：确定性、无模型。
#[derive(Debug, Default, Clone, Copy)]
pub struct LexicalOverlapReranker;

fn tokenize(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

impl Reranker for LexicalOverlapReranker {
    fn rerank(&self, query: &str, candidates: &[String]) -> anyhow::Result<Vec<f64>> {
        let q = tokenize(query);
        if q.is_empty() {
            return Ok(vec![0.0; candidates.len()]);
        }
        Ok(candidates
            .iter()
            .map(|c| {
                let d = tokenize(c);
                let inter = q.intersection(&d).count();
                let union = q.union(&d).count();
                if union == 0 {
                    0.0
                } else {
                    inter as f64 / union as f64
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_overlap() {
        let r = LexicalOverlapReranker;
        let cands = vec![
            "alpha beta gamma".to_string(), // q={alpha,beta} ∩={alpha,beta} ∪={alpha,beta,gamma} =2/3
            "alpha beta".to_string(),       // 完全重叠 =1
            "delta epsilon".to_string(),    // 无重叠 =0
        ];
        let scores = r.rerank("alpha beta", &cands).unwrap();
        assert!((scores[0] - 2.0 / 3.0).abs() < 1e-12);
        assert!((scores[1] - 1.0).abs() < 1e-12);
        assert_eq!(scores[2], 0.0);
    }

    #[test]
    fn empty_query_and_candidates() {
        let r = LexicalOverlapReranker;
        assert_eq!(r.rerank("", &["x".into()]).unwrap(), vec![0.0]);
        assert!(r.rerank("q", &[]).unwrap().is_empty());
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let r = LexicalOverlapReranker;
        let s = r.rerank("Hello, World!", &["hello world".into()]).unwrap();
        assert!((s[0] - 1.0).abs() < 1e-12);
    }
}
