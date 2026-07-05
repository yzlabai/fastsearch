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

/// CJK 表意/假名字符判定：这些字符 `is_alphanumeric()` 为 true，但字间无空格，
/// 若按"非字母数字"切分会整句成单 token（中文候选 Jaccard 恒 0 → rerank 退化为
/// gid 序，反向破坏融合排名）。故对 CJK 走字符 bigram 切分（无外部分词依赖）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF |   // 平假名 + 片假名
        0x3400..=0x4DBF |   // CJK 扩展 A
        0x4E00..=0x9FFF |   // CJK 统一表意
        0xF900..=0xFAFF |   // CJK 兼容表意
        0x20000..=0x2FA1F) // CJK 扩展 B–F + 兼容补充
}

/// 分词：ASCII/数字按"非字母数字"切成小写词；CJK 连续段切成重叠字符 bigram
/// （单字段退化为 unigram），使中文候选与查询有可比的词面重叠。
fn tokenize(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_ascii(ascii: &mut String, out: &mut HashSet<String>) {
        if !ascii.is_empty() {
            out.insert(std::mem::take(ascii));
        }
    }
    fn flush_cjk(cjk: &mut Vec<char>, out: &mut HashSet<String>) {
        match cjk.len() {
            0 => {}
            1 => {
                out.insert(cjk[0].to_string());
            }
            _ => {
                for w in cjk.windows(2) {
                    out.insert(w.iter().collect());
                }
            }
        }
        cjk.clear();
    }

    for c in s.chars() {
        if is_cjk(c) {
            flush_ascii(&mut ascii, &mut out);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            ascii.extend(c.to_lowercase());
        } else {
            flush_ascii(&mut ascii, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_ascii(&mut ascii, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
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

    #[test]
    fn cjk_bigram_overlap_ranks_relevant_first() {
        // H1-B 回归：查询"毛利率是多少"对含"毛利率"的候选应有正分（bigram 毛利/利率 命中），
        // 无关中文候选应为 0——旧的整句单-token 切分会让二者同为 0、退化 gid 序。
        let r = LexicalOverlapReranker;
        let cands = vec![
            "公司2023年毛利率为38%".to_string(), // 含 毛利/利率
            "今天天气很好适合出门".to_string(),  // 无重叠
        ];
        let s = r.rerank("毛利率是多少", &cands).unwrap();
        assert!(s[0] > 0.0, "相关中文候选应有正分, got {}", s[0]);
        assert_eq!(s[1], 0.0, "无关中文候选应为 0");
        assert!(s[0] > s[1], "相关候选应排在无关候选前");
    }

    #[test]
    fn cjk_bigram_tokens() {
        // 连续 CJK 段切重叠 bigram；单字退化 unigram。
        let t = tokenize("毛利率");
        assert!(t.contains("毛利") && t.contains("利率") && t.len() == 2);
        assert_eq!(tokenize("年"), HashSet::from(["年".to_string()]));
    }

    #[test]
    fn mixed_cjk_ascii_segments() {
        // 中英数混排：ASCII/数字词与 CJK bigram 各自成 token。
        let t = tokenize("毛利率2023Q3");
        assert!(t.contains("毛利") && t.contains("利率")); // CJK bigram
        assert!(t.contains("2023q3")); // ASCII/数字段小写为一词
    }
}
