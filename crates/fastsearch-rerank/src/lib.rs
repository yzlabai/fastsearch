//! # fastsearch-rerank
//!
//! 排序管线"宽召回 → rerank → top-K"的最后一环。提供 capability 自描述、类型化输入和
//! 显式 [`RerankOutcome::Skipped`] 的 [`Reranker`] trait，以及一个**确定性、零依赖、text-only**
//! 的词项重叠基线 [`LexicalOverlapReranker`]。
//! 架构决策：RAG 主路径默认不上神经 rerank（答案层 LLM 已做联合打分）；trait 为可选
//! 精度档，服务无-LLM 入口时优先纯 Rust 轻量 LTR。详见 [spec](../../docs/specs/21-rerank.md)。

use std::collections::HashSet;

/// reranker 可接受的单个输入类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankInputKind {
    Text,
    Image,
    TextImage,
}

/// reranker 的实测/声明能力。
///
/// `text` 表示 Text→Text，`image` 表示 Image→Image，`cross_modal` 表示不同类型之间
/// 或 TextImage 组合。编排层在调用前用 [`supports`](Self::supports) 做整批准入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerankCaps {
    pub text: bool,
    pub image: bool,
    pub cross_modal: bool,
}

impl RerankCaps {
    pub const fn text_only() -> Self {
        Self {
            text: true,
            image: false,
            cross_modal: false,
        }
    }

    pub fn supports(
        self,
        query: RerankInputKind,
        candidates: impl IntoIterator<Item = RerankInputKind>,
    ) -> bool {
        self.admit(query, candidates).is_ok()
    }

    /// 对一整批输入做 capability 准入，并区分 query 与 candidate 失败原因。
    pub fn admit(
        self,
        query: RerankInputKind,
        candidates: impl IntoIterator<Item = RerankInputKind>,
    ) -> Result<(), RerankSkipReason> {
        let query_supported = match query {
            RerankInputKind::Text => self.text || self.cross_modal,
            RerankInputKind::Image => self.image || self.cross_modal,
            RerankInputKind::TextImage => self.cross_modal,
        };
        if !query_supported {
            return Err(RerankSkipReason::UnsupportedQueryModality);
        }
        if candidates
            .into_iter()
            .any(|candidate| !self.supports_pair(query, candidate))
        {
            return Err(RerankSkipReason::UnsupportedCandidateModality);
        }
        Ok(())
    }

    fn supports_pair(self, query: RerankInputKind, candidate: RerankInputKind) -> bool {
        match (query, candidate) {
            (RerankInputKind::Text, RerankInputKind::Text) => self.text,
            (RerankInputKind::Image, RerankInputKind::Image) => self.image,
            _ => self.cross_modal,
        }
    }
}

/// 一条类型化 rerank 输入。图片字节允许暂缺：能力判断只依赖 kind，具体图片后端可在
/// `rerank` 时把缺字节裁成 [`RerankOutcome::Skipped`] 或错误。
#[derive(Debug, Clone, Copy)]
pub struct RerankInput<'a> {
    kind: RerankInputKind,
    text: Option<&'a str>,
    image: Option<&'a [u8]>,
}

impl<'a> RerankInput<'a> {
    pub const fn text(text: &'a str) -> Self {
        Self {
            kind: RerankInputKind::Text,
            text: Some(text),
            image: None,
        }
    }

    pub const fn image(image: Option<&'a [u8]>) -> Self {
        Self {
            kind: RerankInputKind::Image,
            text: None,
            image,
        }
    }

    pub const fn text_image(text: &'a str, image: &'a [u8]) -> Self {
        Self {
            kind: RerankInputKind::TextImage,
            text: Some(text),
            image: Some(image),
        }
    }

    pub const fn kind(self) -> RerankInputKind {
        self.kind
    }

    pub const fn text_value(self) -> Option<&'a str> {
        self.text
    }

    pub const fn image_value(self) -> Option<&'a [u8]> {
        self.image
    }
}

/// reranker 主动拒绝本批次的稳定原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankSkipReason {
    UnsupportedQueryModality,
    UnsupportedCandidateModality,
    EmptyQueryTokens,
}

impl RerankSkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedQueryModality => "unsupported_query_modality",
            Self::UnsupportedCandidateModality => "unsupported_candidate_modality",
            Self::EmptyQueryTokens => "empty_query_tokens",
        }
    }
}

/// 一次 rerank 的结果。`Skipped` 与空分数不同：它要求编排层保留原排名。
#[derive(Debug, Clone, PartialEq)]
pub enum RerankOutcome {
    Scores(Vec<f64>),
    Skipped(RerankSkipReason),
}

/// rerank 后端。
pub trait Reranker {
    /// 后端可接受的 query/candidate 类型组合。
    fn caps(&self) -> RerankCaps;

    /// 对每个候选返回相关分（与输入同序），或显式声明本批次无信息量/不适用。
    fn rerank(
        &self,
        query: RerankInput<'_>,
        candidates: &[RerankInput<'_>],
    ) -> anyhow::Result<RerankOutcome>;
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
    fn caps(&self) -> RerankCaps {
        RerankCaps::text_only()
    }

    fn rerank(
        &self,
        query: RerankInput<'_>,
        candidates: &[RerankInput<'_>],
    ) -> anyhow::Result<RerankOutcome> {
        if query.kind() != RerankInputKind::Text {
            return Ok(RerankOutcome::Skipped(
                RerankSkipReason::UnsupportedQueryModality,
            ));
        }
        if candidates
            .iter()
            .any(|candidate| candidate.kind() != RerankInputKind::Text)
        {
            return Ok(RerankOutcome::Skipped(
                RerankSkipReason::UnsupportedCandidateModality,
            ));
        }

        let q = tokenize(query.text_value().unwrap_or_default());
        if q.is_empty() {
            return Ok(RerankOutcome::Skipped(RerankSkipReason::EmptyQueryTokens));
        }
        Ok(RerankOutcome::Scores(
            candidates
                .iter()
                .map(|c| {
                    let d = tokenize(c.text_value().unwrap_or_default());
                    let inter = q.intersection(&d).count();
                    let union = q.union(&d).count();
                    if union == 0 {
                        0.0
                    } else {
                        inter as f64 / union as f64
                    }
                })
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_scores(query: &str, candidates: &[String]) -> Vec<f64> {
        let inputs = candidates
            .iter()
            .map(|candidate| RerankInput::text(candidate))
            .collect::<Vec<_>>();
        match LexicalOverlapReranker
            .rerank(RerankInput::text(query), &inputs)
            .unwrap()
        {
            RerankOutcome::Scores(scores) => scores,
            RerankOutcome::Skipped(reason) => panic!("unexpected skip: {reason:?}"),
        }
    }

    #[test]
    fn lexical_capabilities_and_empty_tokens_are_explicit() {
        let r = LexicalOverlapReranker;
        assert_eq!(r.caps(), RerankCaps::text_only());
        assert!(r
            .caps()
            .supports(RerankInputKind::Text, [RerankInputKind::Text]));
        assert!(!r
            .caps()
            .supports(RerankInputKind::Image, [RerankInputKind::Image]));
        assert_eq!(
            r.caps()
                .admit(RerankInputKind::Image, [RerankInputKind::Image]),
            Err(RerankSkipReason::UnsupportedQueryModality)
        );
        assert_eq!(
            r.caps()
                .admit(RerankInputKind::Text, [RerankInputKind::Image]),
            Err(RerankSkipReason::UnsupportedCandidateModality)
        );

        let candidates = [RerankInput::text("alpha")];
        assert_eq!(
            r.rerank(RerankInput::text("!!!"), &candidates).unwrap(),
            RerankOutcome::Skipped(RerankSkipReason::EmptyQueryTokens)
        );
    }

    #[test]
    fn jaccard_overlap() {
        let cands = vec![
            "alpha beta gamma".to_string(), // q={alpha,beta} ∩={alpha,beta} ∪={alpha,beta,gamma} =2/3
            "alpha beta".to_string(),       // 完全重叠 =1
            "delta epsilon".to_string(),    // 无重叠 =0
        ];
        let scores = text_scores("alpha beta", &cands);
        assert!((scores[0] - 2.0 / 3.0).abs() < 1e-12);
        assert!((scores[1] - 1.0).abs() < 1e-12);
        assert_eq!(scores[2], 0.0);
    }

    #[test]
    fn empty_query_and_candidates() {
        let empty_query_candidates = [RerankInput::text("x")];
        assert_eq!(
            LexicalOverlapReranker
                .rerank(RerankInput::text(""), &empty_query_candidates)
                .unwrap(),
            RerankOutcome::Skipped(RerankSkipReason::EmptyQueryTokens)
        );
        assert_eq!(text_scores("q", &[]), Vec::<f64>::new());
    }

    #[test]
    fn case_and_punctuation_insensitive() {
        let s = text_scores("Hello, World!", &["hello world".into()]);
        assert!((s[0] - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cjk_bigram_overlap_ranks_relevant_first() {
        // H1-B 回归：查询"毛利率是多少"对含"毛利率"的候选应有正分（bigram 毛利/利率 命中），
        // 无关中文候选应为 0——旧的整句单-token 切分会让二者同为 0、退化 gid 序。
        let cands = vec![
            "公司2023年毛利率为38%".to_string(), // 含 毛利/利率
            "今天天气很好适合出门".to_string(),  // 无重叠
        ];
        let s = text_scores("毛利率是多少", &cands);
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
