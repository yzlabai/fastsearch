//! HNSW 近似最近邻后端（[`HnswVectorIndex`]）——规模档（10^6+ 向量亚线性查询）。
//!
//! 与暴力 [`MemVectorIndex`](crate::MemVectorIndex) 同实现 [`VectorBackend`]，**默认不启用**。
//! 详见 [设计](../../../docs/plans/2026-06-26-A9-向量HNSW与量化设计.md)。
//!
//! **不变量取舍（诚实记账）**：
//! - 精度/ACL **精确**：HNSW 取回候选后仍用 `Filter::eval` + `AclFilter::visible` 精确后过滤，
//!   并用全精度 f32 重算余弦——结果是暴力精确结果的**子集**，绝不越权、绝不过返。
//! - 召回 **近似**：ANN 固有。靠 `over_fetch`（取回 `k×over_fetch` 候选）+ `ef_search` 调高召回；
//!   强过滤下召回可能下降（候选被过滤殆尽）——见设计 §4，小集合回退暴力为下一迭代。
//!
//! 本步（A9 step 2）：f32 + 增量 insert + 墓碑删除 + over-fetch 后过滤 + 全精度重排。
//! 量化（int8）/持久化（file_dump）/固定种子确定性 为后续步骤。

use crate::{dot, normalize, VecMeta, VectorBackend};
use fastsearch_core::{AclFilter, Filter, GlobalId, Scored};
use hnsw_rs::prelude::{DistCosine, Hnsw};
use std::collections::HashMap;

/// HNSW 构建/检索参数。
#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    /// 每层最大连接数 M（影响图质量/内存；必须 ≤ 256）。
    pub max_nb_connection: usize,
    /// 构建期探索宽度 ef_construction（越大图越好、构建越慢）。
    pub ef_construction: usize,
    /// 检索期探索宽度 ef_search（越大召回越高、越慢）。
    pub ef_search: usize,
    /// 最大层数。
    pub max_layer: usize,
    /// 过取系数：实际向 HNSW 请求 `k × over_fetch` 候选，抵消后过滤损耗。
    pub over_fetch: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams {
            max_nb_connection: 16,
            ef_construction: 200,
            ef_search: 64,
            max_layer: 16,
            over_fetch: 8,
        }
    }
}

struct HnswEntry {
    gid: GlobalId,
    vector: Vec<f32>, // 归一化（内积即余弦），供全精度重排
    meta: VecMeta,
}

/// HNSW 后端：增量插入、墓碑删除、over-fetch + 精确后过滤 + 全精度重排。
pub struct HnswVectorIndex {
    params: HnswParams,
    hnsw: Hnsw<'static, f32, DistCosine>,
    /// 内部 DataId(usize) → 条目；墓碑为 None（向量仍留图中，检索时跳过）。
    entries: Vec<Option<HnswEntry>>,
    gid_to_id: HashMap<GlobalId, usize>,
    dim: Option<usize>,
}

impl HnswVectorIndex {
    pub fn new(params: HnswParams) -> Self {
        // max_elements 仅为分配提示（非硬上限），增量插入可超出。
        let hnsw = Hnsw::<f32, DistCosine>::new(
            params.max_nb_connection,
            10_000,
            params.max_layer,
            params.ef_construction,
            DistCosine {},
        );
        HnswVectorIndex {
            params,
            hnsw,
            entries: Vec::new(),
            gid_to_id: HashMap::new(),
            dim: None,
        }
    }

    pub fn len(&self) -> usize {
        self.gid_to_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.gid_to_id.is_empty()
    }

    pub fn dim(&self) -> Option<usize> {
        self.dim
    }
}

impl Default for HnswVectorIndex {
    fn default() -> Self {
        Self::new(HnswParams::default())
    }
}

impl VectorBackend for HnswVectorIndex {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()> {
        match self.dim {
            Some(d) if d != vector.len() => {
                anyhow::bail!("dimension mismatch: index dim {d}, got {}", vector.len())
            }
            None => self.dim = Some(vector.len()),
            _ => {}
        }
        // 更新：墓碑旧 id（向量留图中，检索时跳过），插入新 id。
        if let Some(old) = self.gid_to_id.remove(&gid) {
            self.entries[old] = None;
        }
        let id = self.entries.len();
        let normalized = normalize(&vector);
        self.hnsw.insert((normalized.as_slice(), id));
        self.entries.push(Some(HnswEntry {
            gid: gid.clone(),
            vector: normalized,
            meta,
        }));
        self.gid_to_id.insert(gid, id);
        Ok(())
    }

    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        if let Some(id) = self.gid_to_id.remove(gid) {
            self.entries[id] = None;
        }
        Ok(())
    }

    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        let victims: Vec<GlobalId> = self
            .gid_to_id
            .keys()
            .filter(|g| g.collection == collection && g.doc_id == doc_id)
            .cloned()
            .collect();
        for g in victims {
            if let Some(id) = self.gid_to_id.remove(&g) {
                self.entries[id] = None;
            }
        }
        Ok(())
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> anyhow::Result<Vec<Scored>> {
        match self.dim {
            Some(d) if query.len() != d => {
                anyhow::bail!("query dim {} != index dim {d}", query.len())
            }
            None => return Ok(vec![]), // 空库
            _ => {}
        }
        if k == 0 {
            return Ok(vec![]);
        }
        let q = normalize(query);
        // over-fetch：向 HNSW 多要候选，抵消后过滤损耗（强过滤仍可能不足，见模块文档）。
        let want = k.saturating_mul(self.params.over_fetch).max(k);
        let ef = self.params.ef_search.max(want);
        let neighbours = self.hnsw.search(q.as_slice(), want, ef);

        let mut scored: Vec<Scored> = neighbours
            .into_iter()
            .filter_map(|n| self.entries.get(n.d_id).and_then(|e| e.as_ref()))
            // 精确后过滤：filter + ACL（不放松、不越权）。
            .filter(|e| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|e| acl.is_none_or(|a| a.visible(&e.meta)))
            // 全精度重排：用原始 f32 重算余弦（抵消近似/未来量化误差）。
            .map(|e| Scored {
                id: e.gid.clone(),
                score: dot(&q, &e.vector) as f64,
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        scored.truncate(k);
        Ok(scored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemVectorIndex;
    use fastsearch_core::BBox;

    fn meta(doc: &str, id: u64, acl: Vec<&str>) -> VecMeta {
        VecMeta {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
            kind: "paragraph".into(),
            modality: "text".into(),
            page: 1,
            section_id: 0,
            heading_path: vec![],
            tenant: None,
            acl: acl.into_iter().map(String::from).collect(),
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 0.0,
                y1: 0.0,
            },
            time: None,
            media: None,
        }
    }

    fn gid(doc: &str, id: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
        }
    }

    // 确定性伪随机向量（不依赖 rand；线性同余 + 三角函数扰动）。
    fn vec_for(seed: u64, dim: usize) -> Vec<f32> {
        let mut s = seed
            .wrapping_mul(2862933555777941757)
            .wrapping_add(3037000493);
        (0..dim)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0
            })
            .collect()
    }

    #[test]
    fn recall_vs_brute_force() {
        let dim = 32;
        let n = 1500;
        let params = HnswParams::default();
        let mut hnsw = HnswVectorIndex::new(params);
        let mut brute = MemVectorIndex::new();
        for i in 0..n {
            let v = vec_for(i as u64, dim);
            hnsw.upsert(
                gid("d", i as u64),
                v.clone(),
                meta("d", i as u64, vec!["public"]),
            )
            .unwrap();
            brute
                .upsert(gid("d", i as u64), v, meta("d", i as u64, vec!["public"]))
                .unwrap();
        }
        assert_eq!(hnsw.len(), n);

        let k = 10;
        let mut hits = 0usize;
        let queries = 50;
        for qi in 0..queries {
            let q = vec_for(100_000 + qi as u64, dim);
            let truth: std::collections::HashSet<_> = brute
                .search(&q, k, None, None)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            let got = hnsw.search(&q, k, None, None).unwrap();
            assert!(got.len() <= k);
            hits += got.iter().filter(|s| truth.contains(&s.id)).count();
        }
        let recall = hits as f64 / (k * queries) as f64;
        assert!(recall >= 0.95, "recall@{k} = {recall} < 0.95");
    }

    #[test]
    fn filter_aware_is_subset_of_exact() {
        use fastsearch_core::AclFilter;
        let dim = 16;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        let mut brute = MemVectorIndex::new();
        for i in 0..400u64 {
            let v = vec_for(i, dim);
            let acl = if i % 2 == 0 {
                vec!["team-a"]
            } else {
                vec!["team-b"]
            };
            hnsw.upsert(gid("d", i), v.clone(), meta("d", i, acl.clone()))
                .unwrap();
            brute.upsert(gid("d", i), v, meta("d", i, acl)).unwrap();
        }
        let acl = AclFilter {
            tenant: None,
            allowed_tags: vec!["team-a".into()],
        };
        let q = vec_for(999, dim);
        let got = hnsw.search(&q, 10, None, Some(&acl)).unwrap();
        // ACL 精确：结果全部可见（team-a，偶数 id），且是暴力精确结果的子集。
        let exact: std::collections::HashSet<_> = brute
            .search(&q, 10, None, Some(&acl))
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert!(!got.is_empty());
        for s in &got {
            assert_eq!(s.id.chunk_id % 2, 0, "越权命中（应只见 team-a 偶数 id）");
            assert!(exact.contains(&s.id), "HNSW 命中应 ⊆ 暴力精确结果");
        }
    }

    #[test]
    fn upsert_updates_and_delete_tombstones() {
        let dim = 8;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        let v = vec_for(1, dim);
        hnsw.upsert(gid("d", 1), v.clone(), meta("d", 1, vec!["public"]))
            .unwrap();
        hnsw.upsert(gid("d", 2), vec_for(2, dim), meta("d", 2, vec!["public"]))
            .unwrap();
        assert_eq!(hnsw.len(), 2);
        // 更新同 gid → 仍 2 条（旧 id 墓碑）
        hnsw.upsert(gid("d", 1), v.clone(), meta("d", 1, vec!["public"]))
            .unwrap();
        assert_eq!(hnsw.len(), 2);
        // 删除 → 检索不再返回
        hnsw.delete(&gid("d", 1)).unwrap();
        assert_eq!(hnsw.len(), 1);
        let got = hnsw.search(&v, 10, None, None).unwrap();
        assert!(got.iter().all(|s| s.id != gid("d", 1)), "已删 gid 不应命中");
    }
}
