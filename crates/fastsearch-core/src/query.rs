//! 查询 AST：结构化的检索请求 + 校验。

use crate::error::{CoreError, Result};
use crate::filter::Filter;
use crate::fusion::Fusion;
use serde::{Deserialize, Serialize};

/// 检索模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Keyword,
    Vector,
    #[default]
    Hybrid,
}

/// rerank 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RerankSpec {
    pub model: String,
    #[serde(default = "default_rerank_top_k")]
    pub top_k: usize,
}

fn default_rerank_top_k() -> usize {
    20
}

/// 分组折叠：每个分组键最多保留 `max_per_group` 条（按最终排名取高分者），
/// 防单文档/单段刷屏。`field` 当前支持 `doc_id` / `section_id`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collapse {
    pub field: String,
    #[serde(default = "default_collapse_max")]
    pub max_per_group: usize,
}

fn default_collapse_max() -> usize {
    1
}

/// 结构化检索请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default)]
    pub fusion: Fusion,
    #[serde(default)]
    pub filter: Option<Filter>,
    /// 外部提供的查询向量；None 则需 embedder 现算。
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    /// 以图搜图查询图字节（MM9）：`vector` 为 None 时，引擎用**支持图像**的后端嵌成查询向量，
    /// 走现有向量召回。真跨模态（文→图）另需后端 `caps.cross_modal`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_image: Option<Vec<u8>>,
    #[serde(default)]
    pub embedder: Option<String>,
    #[serde(default = "default_candidates")]
    pub candidates: usize,
    /// HNSW 检索期探索宽度 `ef_search` 的**逐查询覆盖**（None=用后端配置默认）。越大召回越高、越慢——
    /// 画 recall-vs-QPS 曲线时固定索引、只转此钮即可（暴力/pgvector 档忽略；仅 HNSW 档生效）。
    #[serde(default)]
    pub ef_search: Option<usize>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub rerank: Option<RerankSpec>,
    #[serde(default)]
    pub auto_merge: bool,
    /// 分组折叠（None=不折叠）。
    #[serde(default)]
    pub collapse: Option<Collapse>,
    /// 深分页游标：只返回最终排名中**严格排在该游标之后**的命中（不透明 token，取自上一页
    /// 末条命中的 `cursor()`）。None=第一页。深度受 `candidates` 候选窗口约束（见 engine）。
    #[serde(default)]
    pub search_after: Option<String>,
    #[serde(default)]
    pub highlight: bool,
    /// 在每条命中中附带完整 chunk 正文。默认关闭，避免放大响应。
    #[serde(default)]
    pub include_text: bool,
    /// 在每条命中中附带调用方透传 metadata。默认关闭。
    #[serde(default)]
    pub include_metadata: bool,
    /// 请求分面的字段（当前支持 `kind` / `doc_id`）。
    #[serde(default)]
    pub facets: Vec<String>,
    #[serde(default)]
    pub explain: bool,
}

fn default_candidates() -> usize {
    150
}
fn default_top_k() -> usize {
    20
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            query: String::new(),
            mode: SearchMode::default(),
            fusion: Fusion::default(),
            filter: None,
            vector: None,
            query_image: None,
            embedder: None,
            candidates: default_candidates(),
            ef_search: None,
            top_k: default_top_k(),
            rerank: None,
            auto_merge: false,
            collapse: None,
            search_after: None,
            highlight: false,
            include_text: false,
            include_metadata: false,
            facets: Vec::new(),
            explain: false,
        }
    }
}

/// `top_k` 上界：防客户端可控的巨值经 `Vec::with_capacity` 触发 OOM abort（不可 unwind →
/// 打崩整个多租户服务）。远超任何真实检索需求。
pub const MAX_TOP_K: usize = 10_000;
/// `candidates`（over-fetch 候选窗口）上界：直达后端 `Vec::with_capacity`，必须收口。
pub const MAX_CANDIDATES: usize = 100_000;

impl SearchRequest {
    /// 契约校验：非法组合返回 [`CoreError::InvalidRequest`]。
    pub fn validate(&self) -> Result<()> {
        if self.top_k == 0 {
            return Err(CoreError::InvalidRequest("top_k must be > 0".into()));
        }
        if self.top_k > MAX_TOP_K {
            return Err(CoreError::InvalidRequest(format!(
                "top_k must be <= {MAX_TOP_K}"
            )));
        }
        if self.candidates > MAX_CANDIDATES {
            return Err(CoreError::InvalidRequest(format!(
                "candidates must be <= {MAX_CANDIDATES}"
            )));
        }
        if self.candidates < self.top_k {
            return Err(CoreError::InvalidRequest(
                "candidates must be >= top_k".into(),
            ));
        }
        match &self.fusion {
            Fusion::Rrf { rank_constant } if *rank_constant <= 0.0 => {
                return Err(CoreError::InvalidRequest(
                    "rank_constant must be > 0".into(),
                ));
            }
            Fusion::Normalized { semantic_ratio } if !(0.0..=1.0).contains(semantic_ratio) => {
                return Err(CoreError::InvalidRequest(
                    "semantic_ratio must be in [0,1]".into(),
                ));
            }
            Fusion::Weighted { alpha } if !(0.0..=1.0).contains(alpha) => {
                return Err(CoreError::InvalidRequest("alpha must be in [0,1]".into()));
            }
            _ => {}
        }
        if let Some(r) = &self.rerank {
            if r.top_k == 0 {
                return Err(CoreError::InvalidRequest("rerank.top_k must be > 0".into()));
            }
        }
        if let Some(c) = &self.collapse {
            if c.max_per_group == 0 {
                return Err(CoreError::InvalidRequest(
                    "collapse.max_per_group must be > 0".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let r = SearchRequest {
            query: "hi".into(),
            ..Default::default()
        };
        assert_eq!(r.mode, SearchMode::Hybrid);
        assert_eq!(r.candidates, 150);
        assert_eq!(r.top_k, 20);
        assert!(matches!(r.fusion, Fusion::Rrf { rank_constant } if rank_constant == 60.0));
        assert!(r.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad() {
        let bad_topk = SearchRequest {
            top_k: 0,
            ..Default::default()
        };
        assert!(bad_topk.validate().is_err());

        let bad_cand = SearchRequest {
            top_k: 50,
            candidates: 10,
            ..Default::default()
        };
        assert!(bad_cand.validate().is_err());

        let bad_ratio = SearchRequest {
            fusion: Fusion::Normalized {
                semantic_ratio: 1.5,
            },
            ..Default::default()
        };
        assert!(bad_ratio.validate().is_err());

        let bad_rank = SearchRequest {
            fusion: Fusion::Rrf { rank_constant: 0.0 },
            ..Default::default()
        };
        assert!(bad_rank.validate().is_err());

        // H4 回归：巨值 candidates/top_k 直达后端 `Vec::with_capacity` → OOM abort 打崩服务。
        // validate 必须在此收口，拒绝而非放行。
        let huge_cand = SearchRequest {
            top_k: 1,
            candidates: 100_000_000_000,
            ..Default::default()
        };
        assert!(huge_cand.validate().is_err());
        let huge_topk = SearchRequest {
            top_k: 100_000_000_000,
            candidates: 100_000_000_000,
            ..Default::default()
        };
        assert!(huge_topk.validate().is_err());
        // 边界：恰好上限应通过
        let at_max = SearchRequest {
            top_k: MAX_TOP_K,
            candidates: MAX_CANDIDATES,
            ..Default::default()
        };
        assert!(at_max.validate().is_ok());
    }

    #[test]
    fn request_deserializes_with_defaults() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"毛利率"}"#).unwrap();
        assert_eq!(r.query, "毛利率");
        assert_eq!(r.mode, SearchMode::Hybrid);
        assert_eq!(r.top_k, 20);
    }
}
