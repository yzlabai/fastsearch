//! 混合检索的融合算法：RRF / 分数归一化 / 加权凸组合。
//!
//! 三法都是 fastsearch 的"一等内置"（对位 ParadeDB/VectorChord/pg_textsearch
//! 只能手写 SQL）。所有融合保证**确定性**：同分按 [`GlobalId`] 升序 tie-break，
//! 打乱输入顺序结果一致。

use crate::model::GlobalId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 一条带分的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub id: GlobalId,
    pub score: f64,
}

/// 融合策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Fusion {
    /// Reciprocal Rank Fusion：`Σ 1/(k+rank)`，rank 从 1 起。尺度无关、稳健。
    Rrf {
        #[serde(default = "default_rank_constant")]
        rank_constant: f64,
    },
    /// 各路 min-max 归一化到 [0,1] 后按 `semantic_ratio` 加权。
    Normalized {
        #[serde(default = "default_semantic_ratio")]
        semantic_ratio: f64,
    },
    /// 加权凸组合：`alpha*dense + (1-alpha)*sparse`（先各自 min-max 归一化）。
    Weighted {
        #[serde(default = "default_alpha")]
        alpha: f64,
    },
}

fn default_rank_constant() -> f64 {
    60.0
}
fn default_semantic_ratio() -> f64 {
    0.5
}
fn default_alpha() -> f64 {
    0.5
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Rrf {
            rank_constant: 60.0,
        }
    }
}

/// 融合 keyword 与 semantic 两路候选，返回按融合分降序、确定性 tie-break 的结果。
///
/// - 输入各自可乱序；本函数内部按分排名。
/// - 一路为空时退化为另一路的相应融合。
pub fn fuse(keyword: &[Scored], semantic: &[Scored], fusion: &Fusion) -> Vec<Scored> {
    let mut acc: HashMap<GlobalId, f64> = HashMap::new();
    match fusion {
        Fusion::Rrf { rank_constant } => {
            for path in [keyword, semantic] {
                for (rank, s) in rank_desc(path).iter().enumerate() {
                    let contrib = 1.0 / (rank_constant + (rank as f64 + 1.0));
                    *acc.entry(s.id.clone()).or_insert(0.0) += contrib;
                }
            }
        }
        Fusion::Normalized { semantic_ratio } => {
            let kw = normalize(keyword);
            let sem = normalize(semantic);
            for s in &kw {
                *acc.entry(s.id.clone()).or_insert(0.0) += (1.0 - semantic_ratio) * s.score;
            }
            for s in &sem {
                *acc.entry(s.id.clone()).or_insert(0.0) += semantic_ratio * s.score;
            }
        }
        Fusion::Weighted { alpha } => {
            let kw = normalize(keyword);
            let sem = normalize(semantic);
            for s in &kw {
                *acc.entry(s.id.clone()).or_insert(0.0) += (1.0 - alpha) * s.score;
            }
            for s in &sem {
                *acc.entry(s.id.clone()).or_insert(0.0) += alpha * s.score;
            }
        }
    }
    sort_scored(acc)
}

/// 按分降序排名（用于 RRF 取 rank）；同分按 id 升序保证确定性。
fn rank_desc(items: &[Scored]) -> Vec<Scored> {
    let mut v = items.to_vec();
    v.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    v
}

/// min-max 归一化到 [0,1]。空→空；单元素或全同值→全 1.0（避免除零、视作满分）。
fn normalize(items: &[Scored]) -> Vec<Scored> {
    if items.is_empty() {
        return vec![];
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for s in items {
        lo = lo.min(s.score);
        hi = hi.max(s.score);
    }
    let span = hi - lo;
    items
        .iter()
        .map(|s| Scored {
            id: s.id.clone(),
            score: if span <= f64::EPSILON {
                1.0
            } else {
                (s.score - lo) / span
            },
        })
        .collect()
}

/// 把累加分排序成确定性结果（分降序，同分 id 升序）。
fn sort_scored(acc: HashMap<GlobalId, f64>) -> Vec<Scored> {
    let mut out: Vec<Scored> = acc
        .into_iter()
        .map(|(id, score)| Scored { id, score })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: "d".into(),
            chunk_id: n,
        }
    }
    fn s(n: u64, score: f64) -> Scored {
        Scored { id: id(n), score }
    }

    #[test]
    fn rrf_known_values() {
        // keyword: [id1 rank1, id2 rank2]；semantic: [id2 rank1, id3 rank2]；k=60
        let kw = vec![s(1, 10.0), s(2, 5.0)];
        let sem = vec![s(2, 0.9), s(3, 0.8)];
        let out = fuse(
            &kw,
            &sem,
            &Fusion::Rrf {
                rank_constant: 60.0,
            },
        );
        // id2 = 1/62 + 1/61 ; id1 = 1/61 ; id3 = 1/62
        let m: std::collections::HashMap<_, _> =
            out.iter().map(|x| (x.id.chunk_id, x.score)).collect();
        let i2 = 1.0 / 62.0 + 1.0 / 61.0;
        let i1 = 1.0 / 61.0;
        let i3 = 1.0 / 62.0;
        assert!((m[&2] - i2).abs() < 1e-12);
        assert!((m[&1] - i1).abs() < 1e-12);
        assert!((m[&3] - i3).abs() < 1e-12);
        // 排序：id2 最高，其次 id1，再 id3
        assert_eq!(out[0].id.chunk_id, 2);
        assert_eq!(out[1].id.chunk_id, 1);
        assert_eq!(out[2].id.chunk_id, 3);
    }

    #[test]
    fn normalized_basic() {
        let kw = vec![s(1, 0.0), s(2, 10.0)]; // 归一化 → 0 和 1
        let sem = vec![s(1, 100.0), s(2, 100.0)]; // 全同值 → 全 1.0
        let out = fuse(
            &kw,
            &sem,
            &Fusion::Normalized {
                semantic_ratio: 0.5,
            },
        );
        let m: std::collections::HashMap<_, _> =
            out.iter().map(|x| (x.id.chunk_id, x.score)).collect();
        // id1 = 0.5*0 + 0.5*1 = 0.5 ; id2 = 0.5*1 + 0.5*1 = 1.0
        assert!((m[&1] - 0.5).abs() < 1e-12);
        assert!((m[&2] - 1.0).abs() < 1e-12);
        assert_eq!(out[0].id.chunk_id, 2);
    }

    #[test]
    fn one_path_empty_degrades() {
        let kw = vec![s(1, 5.0), s(2, 3.0)];
        let out = fuse(
            &kw,
            &[],
            &Fusion::Rrf {
                rank_constant: 60.0,
            },
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id.chunk_id, 1); // 高分在前
    }

    #[test]
    fn deterministic_under_shuffle() {
        let kw1 = vec![s(1, 5.0), s(2, 5.0), s(3, 1.0)];
        let kw2 = vec![s(3, 1.0), s(2, 5.0), s(1, 5.0)]; // 打乱
        let f = Fusion::Rrf {
            rank_constant: 60.0,
        };
        let a = fuse(&kw1, &[], &f);
        let b = fuse(&kw2, &[], &f);
        assert_eq!(a, b);
        // 同分（id1,id2 都 5.0）→ tie-break 按 id 升序：id1 在 id2 前
        assert_eq!(a[0].id.chunk_id, 1);
        assert_eq!(a[1].id.chunk_id, 2);
    }

    #[test]
    fn fusion_serde() {
        let f: Fusion = serde_json::from_str(r#"{"method":"rrf","rank_constant":60}"#).unwrap();
        assert_eq!(
            f,
            Fusion::Rrf {
                rank_constant: 60.0
            }
        );
        let f2: Fusion = serde_json::from_str(r#"{"method":"normalized"}"#).unwrap();
        assert_eq!(
            f2,
            Fusion::Normalized {
                semantic_ratio: 0.5
            }
        );
    }
}
