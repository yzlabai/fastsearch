//! 混合检索的融合算法：RRF / 分数归一化 / 加权凸组合。
//!
//! 三法都是 fastsearch 的"一等内置"（对位 ParadeDB/VectorChord/pg_textsearch
//! 只能手写 SQL）。所有融合保证**确定性**：同分按 [`GlobalId`] 升序 tie-break，
//! 打乱输入顺序结果一致。

use crate::model::GlobalId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

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
    /// 各路 min-max 归一化后按具名来源加权。未列出来源用 `default_weight`。
    ///
    /// 这是 N 路加权的公开表达；`Normalized` / `Weighted` 保留为两路兼容形状。
    Weights {
        #[serde(default)]
        weights: BTreeMap<String, f64>,
        #[serde(default = "default_weight")]
        default_weight: f64,
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
fn default_weight() -> f64 {
    1.0
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Rrf {
            rank_constant: 60.0,
        }
    }
}

/// 一路召回：**带来源标识与权重**的候选列表（KB-2.2）。
///
/// `source` 形如 `<召回方式>:<信号>`（如 `keyword:user_text` / `vector:image_caption`），
/// 与 [KB-2.1](../../../docs/plans/2026-08-25-chunk-signal多表示设计.md) 的 `signal_type` 对齐。
#[derive(Debug, Clone, PartialEq)]
pub struct RecallList {
    pub source: String,
    /// 该路权重。**仅加权档用**（RRF 是尺度无关的排名融合，忽略它）。
    pub weight: f64,
    pub items: Vec<Scored>,
}

impl RecallList {
    pub fn new(source: impl Into<String>, weight: f64, items: Vec<Scored>) -> Self {
        RecallList {
            source: source.into(),
            weight,
            items,
        }
    }
}

/// 某条召回路对一条最终命中的可解释贡献。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceHit {
    /// 具名来源，如 `keyword:user_text` / `vector:image_caption`。
    pub source: String,
    /// 该路内名次，1 起。
    pub rank: usize,
    /// 该路原始分（归一化前）。
    pub score: f64,
    /// 该路对最终融合分的实际加数。
    pub contribution: f64,
}

/// **N 路**具名融合（KB-2.2 第一步）。[`fuse`] 是它的两路特例。
///
/// 与两路版的关系：
/// - `Rrf` 天然 N 路——原实现就是 `for path in [keyword, semantic]` 的循环，这里换成遍历 `lists`。
/// - 加权档（`Normalized`/`Weighted`）的标量在 N 路下没有意义，**改用每路自带的 `weight`**：
///   两路时 `fuse` 把标量翻译成 `(1-x)` / `x` 两个权重，于是行为**逐位不变**。
///   （这两个档在 `fuse` 里的实现本就逐字同构，是 2026-07-05 审查记录在案的旧账；
///   本函数只用一份加权逻辑，不把重复搬进 N 路。）
///
/// **确定性（不变量 #4）**：按 `source` 字典序遍历各路再累加浮点。
/// 用输入顺序累加会让"同一组召回、不同拼装顺序"产生不同的浮点尾数——
/// 两路时只有两个加数看不出来，N 路化正是这个坑冒头的地方。
/// `source` 是一路的唯一身份；重复 source 属于调用方编排错误，会立即拒绝。
pub fn fuse_n(lists: &[RecallList], fusion: &Fusion) -> Vec<Scored> {
    fuse_n_with_sources(lists, fusion).0
}

/// N 路具名融合，同时返回每条命中的来源、路内名次、原始分和贡献。
///
/// `BTreeMap` 仅用于给调用方稳定访问；每条的 `Vec<SourceHit>` 按 `source`
/// 字典序排列，与浮点累加顺序相同。
pub fn fuse_n_with_sources(
    lists: &[RecallList],
    fusion: &Fusion,
) -> (Vec<Scored>, BTreeMap<GlobalId, Vec<SourceHit>>) {
    // 借用排序：不克隆 items（各路可能很大）。
    let mut order: Vec<&RecallList> = lists.iter().collect();
    order.sort_by(|a, b| a.source.cmp(&b.source));
    assert!(
        order
            .windows(2)
            .all(|pair| pair[0].source != pair[1].source),
        "duplicate recall source"
    );

    let mut acc: HashMap<GlobalId, f64> = HashMap::new();
    let mut sources: BTreeMap<GlobalId, Vec<SourceHit>> = BTreeMap::new();
    match fusion {
        Fusion::Rrf { rank_constant } => {
            for l in order {
                for (rank, s) in rank_desc(&l.items).iter().enumerate() {
                    let contrib = 1.0 / (rank_constant + (rank as f64 + 1.0));
                    *acc.entry(s.id.clone()).or_insert(0.0) += contrib;
                    sources.entry(s.id.clone()).or_default().push(SourceHit {
                        source: l.source.clone(),
                        rank: rank + 1,
                        score: s.score,
                        contribution: contrib,
                    });
                }
            }
        }
        // 两个加权档数学等价（旧账，见上）⇒ 共用一份实现，权重来自每路的 `weight`。
        Fusion::Normalized { .. } | Fusion::Weighted { .. } | Fusion::Weights { .. } => {
            for l in order {
                let weight = match fusion {
                    Fusion::Weights {
                        weights,
                        default_weight,
                    } => weights.get(&l.source).copied().unwrap_or(*default_weight),
                    _ => l.weight,
                };
                let ranked = rank_desc(&l.items);
                for (rank, (raw, normalized)) in ranked.iter().zip(normalize(&ranked)).enumerate() {
                    let contribution = weight * normalized.score;
                    *acc.entry(raw.id.clone()).or_insert(0.0) += contribution;
                    sources.entry(raw.id.clone()).or_default().push(SourceHit {
                        source: l.source.clone(),
                        rank: rank + 1,
                        score: raw.score,
                        contribution,
                    });
                }
            }
        }
    }
    (sort_scored(acc), sources)
}

/// 融合 keyword 与 semantic 两路候选，返回按融合分降序、确定性 tie-break 的结果。
///
/// - 输入各自可乱序；本函数内部按分排名。
/// - 一路为空时退化为另一路的相应融合。
pub fn fuse(keyword: &[Scored], semantic: &[Scored], fusion: &Fusion) -> Vec<Scored> {
    // 两路 = N 路的特例。加权档的标量在这里翻译成两路权重：keyword 得 `1-x`、semantic 得 `x`
    // （`Normalized.semantic_ratio` 与 `Weighted.alpha` 扮演的是同一个 `x`——它们数学等价）。
    // 来源名取公开规范的 `keyword:user_text`/`vector:user_text`；前者仍按字典序在前，
    // 与原实现的累加顺序一致 ⇒ 浮点结果逐位不变（由 `n_way_matches_two_way_bitwise` 钉住）。
    let x = match fusion {
        Fusion::Rrf { .. } => 0.0, // RRF 忽略权重
        Fusion::Normalized { semantic_ratio } => *semantic_ratio,
        Fusion::Weighted { alpha } => *alpha,
        Fusion::Weights { .. } => 0.0, // 权重由具名表直接提供
    };
    fuse_n(
        &[
            RecallList::new("keyword:user_text", 1.0 - x, keyword.to_vec()),
            RecallList::new("vector:user_text", x, semantic.to_vec()),
        ],
        fusion,
    )
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

    /// FS-002 之前公开两路算法的独立参考实现。不能调用 `fuse_n`，否则兼容测试会自证。
    fn legacy_two_way(keyword: &[Scored], semantic: &[Scored], fusion: &Fusion) -> Vec<Scored> {
        let mut acc = HashMap::new();
        match fusion {
            Fusion::Rrf { rank_constant } => {
                for path in [keyword, semantic] {
                    for (rank, scored) in rank_desc(path).iter().enumerate() {
                        *acc.entry(scored.id.clone()).or_insert(0.0) +=
                            1.0 / (rank_constant + (rank as f64 + 1.0));
                    }
                }
            }
            Fusion::Normalized { semantic_ratio } => {
                for scored in normalize(keyword) {
                    *acc.entry(scored.id).or_insert(0.0) += (1.0 - semantic_ratio) * scored.score;
                }
                for scored in normalize(semantic) {
                    *acc.entry(scored.id).or_insert(0.0) += semantic_ratio * scored.score;
                }
            }
            Fusion::Weighted { alpha } => {
                for scored in normalize(keyword) {
                    *acc.entry(scored.id).or_insert(0.0) += (1.0 - alpha) * scored.score;
                }
                for scored in normalize(semantic) {
                    *acc.entry(scored.id).or_insert(0.0) += alpha * scored.score;
                }
            }
            Fusion::Weights { .. } => unreachable!("新档没有旧两路参考实现"),
        }
        sort_scored(acc)
    }

    // ---- KB-2.2 N 路具名融合 ----------------------------------------------

    /// **本项最重要的一条**：N 路实现喂两路，必须与旧两路实现**逐位相同**。
    ///
    /// 不是"约等于"——浮点累加顺序一变，尾数就会漂，而融合分会传导到最终名次。
    /// 所以断言用 `to_bits()` 比较，不是 `abs() < eps`。
    #[test]
    fn n_way_matches_two_way_bitwise() {
        let kw = vec![s(1, 10.0), s(2, 5.0), s(4, 1.0)];
        let sem = vec![s(2, 0.9), s(3, 0.8), s(5, 0.1)];
        for f in [
            Fusion::Rrf {
                rank_constant: 60.0,
            },
            Fusion::Rrf { rank_constant: 7.5 },
            Fusion::Normalized {
                semantic_ratio: 0.7,
            },
            Fusion::Weighted { alpha: 0.3 },
            Fusion::Normalized {
                semantic_ratio: 0.0,
            },
            Fusion::Weighted { alpha: 1.0 },
        ] {
            let two = legacy_two_way(&kw, &sem, &f);
            let x = match &f {
                Fusion::Rrf { .. } => 0.0,
                Fusion::Normalized { semantic_ratio } => *semantic_ratio,
                Fusion::Weighted { alpha } => *alpha,
                Fusion::Weights { .. } => 0.0,
            };
            let n = fuse_n(
                &[
                    RecallList::new("keyword", 1.0 - x, kw.clone()),
                    RecallList::new("vector", x, sem.clone()),
                ],
                &f,
            );
            assert_eq!(two.len(), n.len(), "{f:?}");
            for (a, b) in two.iter().zip(&n) {
                assert_eq!(a.id, b.id, "{f:?} 名次必须一致");
                assert_eq!(
                    a.score.to_bits(),
                    b.score.to_bits(),
                    "{f:?} 融合分必须**逐位**相同，不能只是接近"
                );
            }
        }
    }

    /// 确定性（不变量 #4）：打乱各路内部顺序**且**打乱路与路的顺序，结果不变。
    ///
    /// 后半条是 N 路化新引入的风险面——两路时只有两个加数，累加顺序看不出差别。
    #[test]
    fn n_way_deterministic_under_list_shuffle() {
        let a = RecallList::new("keyword:user_text", 0.5, vec![s(1, 10.0), s(2, 5.0)]);
        let b = RecallList::new("vector:user_text", 0.3, vec![s(2, 0.9), s(3, 0.8)]);
        let c = RecallList::new("vector:image_caption", 0.2, vec![s(3, 0.5), s(4, 0.4)]);
        for f in [
            Fusion::Rrf {
                rank_constant: 60.0,
            },
            Fusion::Normalized {
                semantic_ratio: 0.5,
            },
            Fusion::Weights {
                weights: BTreeMap::from([
                    ("keyword:user_text".into(), 0.2),
                    ("vector:user_text".into(), 0.3),
                    ("vector:image_caption".into(), 0.5),
                ]),
                default_weight: 1.0,
            },
        ] {
            let base = fuse_n(&[a.clone(), b.clone(), c.clone()], &f);
            let mut a_reversed = a.clone();
            a_reversed.items.reverse();
            let mut b_reversed = b.clone();
            b_reversed.items.reverse();
            let mut c_reversed = c.clone();
            c_reversed.items.reverse();
            for perm in [
                vec![c.clone(), a.clone(), b.clone()],
                vec![b.clone(), c.clone(), a.clone()],
                vec![c.clone(), b.clone(), a.clone()],
                vec![c_reversed.clone(), a_reversed.clone(), b_reversed.clone()],
                vec![b_reversed.clone(), c_reversed.clone(), a_reversed.clone()],
            ] {
                let got = fuse_n(&perm, &f);
                assert_eq!(got.len(), base.len(), "{f:?}");
                for (x, y) in base.iter().zip(&got) {
                    assert_eq!(x.id, y.id, "{f:?} 路顺序不得影响名次");
                    assert_eq!(
                        x.score.to_bits(),
                        y.score.to_bits(),
                        "{f:?} 路顺序不得影响浮点尾数"
                    );
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "duplicate recall source")]
    fn duplicate_source_is_rejected() {
        fuse_n(
            &[
                RecallList::new("vector:user_text", 1.0, vec![s(1, 0.9)]),
                RecallList::new("vector:user_text", 1.0, vec![s(2, 0.8)]),
            ],
            &Fusion::default(),
        );
    }

    #[test]
    fn named_weights_work_through_two_way_compatibility_api() {
        let out = fuse(
            &[s(1, 10.0), s(2, 1.0)],
            &[s(1, 0.1), s(2, 0.9)],
            &Fusion::Weights {
                weights: BTreeMap::from([
                    ("keyword:user_text".into(), 0.0),
                    ("vector:user_text".into(), 1.0),
                ]),
                default_weight: 0.0,
            },
        );
        assert_eq!(out[0].id, id(2));
        assert_eq!(out[0].score, 1.0);
    }

    #[test]
    fn named_weights_single_path_uses_its_named_weight() {
        let out = fuse_n(
            &[RecallList::new(
                "vector:image_caption",
                99.0,
                vec![s(2, 0.9), s(1, 0.1)],
            )],
            &Fusion::Weights {
                weights: BTreeMap::from([("vector:image_caption".into(), 0.4)]),
                default_weight: 1.0,
            },
        );
        assert_eq!(out, vec![s(2, 0.4), s(1, 0.0)]);
    }

    /// 三路 RRF：一个候选被两路命中应压过只被一路命中的。
    #[test]
    fn n_way_rrf_rewards_multi_source_hits() {
        let out = fuse_n(
            &[
                RecallList::new("keyword:user_text", 1.0, vec![s(1, 9.0), s(9, 1.0)]),
                RecallList::new("vector:user_text", 1.0, vec![s(1, 0.9)]),
                RecallList::new("vector:image_caption", 1.0, vec![s(7, 0.5)]),
            ],
            &Fusion::Rrf {
                rank_constant: 60.0,
            },
        );
        assert_eq!(out[0].id, id(1), "被两路命中的应排第一：{out:?}");
        // id9 在 keyword 里排第 2，id7 在 caption 路排第 1 ⇒ id7 应压过 id9。
        let pos = |n: u64| out.iter().position(|x| x.id == id(n)).unwrap();
        assert!(pos(7) < pos(9), "各路第 1 名应压过别路第 2 名：{out:?}");
    }

    /// 加权档在 N 路下用**每路自带的 weight**（标量在 N 路没有意义）。
    #[test]
    fn n_way_weights_come_from_each_list() {
        let lists = |w_img: f64| {
            vec![
                RecallList::new("vector:user_text", 1.0, vec![s(1, 1.0), s(2, 0.0)]),
                RecallList::new("vector:image_caption", w_img, vec![s(2, 1.0), s(1, 0.0)]),
            ]
        };
        let f = Fusion::Normalized {
            semantic_ratio: 0.5, // N 路下该标量被忽略
        };
        // 图片路权重低 → id1 胜；权重高 → id2 胜。方向必须符合预期。
        assert_eq!(fuse_n(&lists(0.1), &f)[0].id, id(1));
        assert_eq!(fuse_n(&lists(9.0), &f)[0].id, id(2));
    }

    /// 空路不影响结果（N 路版的 `one_path_empty_degrades`）。
    #[test]
    fn n_way_empty_lists_are_inert() {
        let real = RecallList::new("keyword:user_text", 1.0, vec![s(1, 10.0), s(2, 5.0)]);
        let f = Fusion::Rrf {
            rank_constant: 60.0,
        };
        let alone = fuse_n(std::slice::from_ref(&real), &f);
        let padded = fuse_n(
            &[
                real.clone(),
                RecallList::new("vector:image_bytes", 1.0, vec![]),
                RecallList::new("zzz:empty", 3.0, vec![]),
            ],
            &f,
        );
        assert_eq!(alone, padded, "空路不得改变任何东西");
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
        let old_weighted: Fusion =
            serde_json::from_str(r#"{"method":"weighted","alpha":0.25}"#).unwrap();
        assert_eq!(old_weighted, Fusion::Weighted { alpha: 0.25 });
        let named: Fusion =
            serde_json::from_str(r#"{"method":"weights","weights":{"keyword:user_text":2.0}}"#)
                .unwrap();
        assert_eq!(
            named,
            Fusion::Weights {
                weights: BTreeMap::from([("keyword:user_text".into(), 2.0)]),
                default_weight: 1.0,
            }
        );
    }

    #[test]
    fn named_weights_explain_rank_raw_score_and_contribution() {
        let fusion: Fusion = serde_json::from_str(
            r#"{"method":"weights","weights":{"keyword:user_text":0.25,"vector:user_text":2.0},"default_weight":0.5}"#,
        )
        .unwrap();
        let (out, sources) = fuse_n_with_sources(
            &[
                RecallList::new("keyword:user_text", 99.0, vec![s(1, 10.0), s(2, 5.0)]),
                RecallList::new("vector:user_text", 99.0, vec![s(2, 0.9), s(3, 0.8)]),
            ],
            &fusion,
        );

        assert_eq!(out[0].id, id(2));
        assert_eq!(out[0].score, 2.0);
        assert_eq!(sources[&id(2)].len(), 2);
        assert_eq!(
            sources[&id(2)][0],
            SourceHit {
                source: "keyword:user_text".into(),
                rank: 2,
                score: 5.0,
                contribution: 0.0,
            }
        );
        assert_eq!(
            sources[&id(2)][1],
            SourceHit {
                source: "vector:user_text".into(),
                rank: 1,
                score: 0.9,
                contribution: 2.0,
            }
        );
    }
}
