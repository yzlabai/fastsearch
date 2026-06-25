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
    #[serde(default)]
    pub embedder: Option<String>,
    #[serde(default = "default_candidates")]
    pub candidates: usize,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default)]
    pub rerank: Option<RerankSpec>,
    #[serde(default)]
    pub auto_merge: bool,
    /// 分组折叠（None=不折叠）。
    #[serde(default)]
    pub collapse: Option<Collapse>,
    #[serde(default)]
    pub highlight: bool,
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
            embedder: None,
            candidates: default_candidates(),
            top_k: default_top_k(),
            rerank: None,
            auto_merge: false,
            collapse: None,
            highlight: false,
            facets: Vec::new(),
            explain: false,
        }
    }
}

impl SearchRequest {
    /// 契约校验：非法组合返回 [`CoreError::InvalidRequest`]。
    pub fn validate(&self) -> Result<()> {
        if self.top_k == 0 {
            return Err(CoreError::InvalidRequest("top_k must be > 0".into()));
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
    }

    #[test]
    fn request_deserializes_with_defaults() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"毛利率"}"#).unwrap();
        assert_eq!(r.query, "毛利率");
        assert_eq!(r.mode, SearchMode::Hybrid);
        assert_eq!(r.top_k, 20);
    }
}
