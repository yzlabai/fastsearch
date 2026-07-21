//! HNSW 近似最近邻后端（[`HnswVectorIndex`]）——规模档（10^6+ 向量亚线性查询）。
//!
//! 与暴力 [`MemVectorIndex`](crate::MemVectorIndex) 同实现 [`VectorBackend`]，**默认不启用**。
//! 详见 [设计](../../../docs/plans/2026-06-26-A9-向量HNSW与量化设计.md)。
//!
//! **不变量取舍（诚实记账）**：
//! - 精度/ACL **精确**：HNSW 取回候选后仍用 `Filter::eval` + `AclFilter::visible` 精确后过滤，
//!   并用全精度 f32 重算余弦——结果是暴力精确结果的**子集**，绝不越权、绝不过返。
//! - 召回 **近似**：ANN 固有。靠 `over_fetch`（取回 `k×over_fetch` 候选）+ `ef_search` 调高召回。
//!   **强选择性过滤**下用 **图内 filtered-traversal**（hnsw_rs `search_filter`：把 filter+ACL 谓词
//!   下推进遍历，结果堆只收合规候选，遍历仍穿过被裁节点保连通）+ **自适应过取**安全网（过滤后不足
//!   `k` 则翻倍 `want` 重搜、上限全集，最坏退化对 filter 精确全扫），不让选择性过滤掉召回（守 #5）。
//!
//! **墓碑回收（守不变量 #6）**：删除/更新只把 `entries[id]` 置 None、向量仍留图中。为防长
//! 生命周期进程在高频 upsert/delete 下 `entries`/图无界增长，删除/更新后按墓碑比例**自动压实**
//! （[`HnswVectorIndex::compact`]：墓碑过半且超下限时，用活条目原地重建图与稠密 id 映射；摊还
//! O(1)）。亦可手动 `compact`，或经 `save`→`load`（只持久化活条目、重建图）压实。
//!
//! **非确定性（诚实记账，触不变量 #4）**：`hnsw_rs` 用 `StdRng::from_os_rng()` 生成层级、
//! **未暴露 seed**，故每次建图不同 → 检索结果有近似抖动、`save`→`load`（重建图）结果可能微异。
//! 这是 ANN 固有性质；默认的暴力 [`MemVectorIndex`](crate::MemVectorIndex) 仍**完全确定**。
//! HNSW 是 opt-in 规模档，其近似/非确定是明示取舍（要确定就用默认档）。
//!
//! 已实现（A9 step 2/3/4/6）：增量 insert + 墓碑删除 + over-fetch 后过滤 + **u8 量化图**
//! （省 ~4× 图内存）+ **全精度 f32 重排**（量化误差兜底，recall@10≈0.99）+ 持久化（存 f32、
//! load 重建图）+ 小集合回退暴力 + 接 engine + **图内 filtered-traversal**（hnsw_rs
//! `search_filter` 谓词下推）+ **自适应过取（强过滤召回兜底）** + **墓碑自动压实**。

use crate::{dot, normalize, VecMeta, VectorBackend};
use fastsearch_core::{AclFilter, Citation, Filter, GlobalId, Scored};
use hnsw_rs::prelude::{DistL2, FilterT, Hnsw};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// 活条目数 ≤ 此值时检索回退暴力精确（保召回=1.0、确定）；超过才用 HNSW 近似图。
const BRUTE_FALLBACK_MAX: usize = 1000;

/// 进程内自动压实下限：`entries`（含墓碑）总量 ≤ 此值不触发压实，避免微小索引在
/// 增删抖动中反复重建图（重建是 O(活条目) re-insert）。超过此量且墓碑过半才压实。
const COMPACT_MIN_TOTAL: usize = 32;

/// 把**归一化** f32 分量（∈[-1,1]）对称量化到 u8（∈[0,255]）：图存 u8（省 ~4× 图内存），
/// 全精度重排仍用旁挂 f32。仿射平移对所有向量一致 → u8 L2 序与原始一致（modulo 取整）。
fn quantize_u8(normalized: &[f32]) -> Vec<u8> {
    normalized
        .iter()
        .map(|&x| (((x + 1.0) * 127.5).round().clamp(0.0, 255.0)) as u8)
        .collect()
}

/// HNSW 构建/检索参数。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
/// 图存 **u8 量化**向量（省内存）；`entries` 旁挂 **f32** 全精度向量供精排/小集合暴力。
pub struct HnswVectorIndex {
    params: HnswParams,
    hnsw: Hnsw<'static, u8, DistL2>,
    /// 内部 DataId(usize) → 条目；墓碑为 None（向量仍留图中，检索时跳过）。
    entries: Vec<Option<HnswEntry>>,
    gid_to_id: HashMap<GlobalId, usize>,
    dim: Option<usize>,
}

impl HnswVectorIndex {
    pub fn new(params: HnswParams) -> Self {
        // max_elements 仅为分配提示（非硬上限），增量插入可超出。
        let hnsw = Hnsw::<u8, DistL2>::new(
            params.max_nb_connection,
            10_000,
            params.max_layer,
            params.ef_construction,
            DistL2 {},
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

    /// 清空全部条目 + 重置图（供单集合原地重建）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.gid_to_id.clear();
        self.dim = None;
        self.hnsw = Hnsw::<u8, DistL2>::new(
            self.params.max_nb_connection,
            10_000,
            self.params.max_layer,
            self.params.ef_construction,
            DistL2 {},
        );
    }

    /// 墓碑数 = 已分配槽位 - 活条目数（删除/更新只置 `None`，向量仍留图中）。
    pub fn dead_count(&self) -> usize {
        self.entries.len() - self.gid_to_id.len()
    }

    /// 进程内压实：丢弃墓碑，用**活条目**重建图与稠密 id 映射（绕 hnsw_rs 无原地删除）。
    /// 等价 `save`→`load` 的活集重建，但全程内存、不落盘——回收墓碑占用的图内存/槽位。
    /// 活集与检索语义不变（图内部 id 重排；HNSW 近似抖动本就是其固有性质，见模块注）。
    pub fn compact(&mut self) {
        if self.dead_count() == 0 {
            return; // 无墓碑，免重建
        }
        let live: Vec<HnswEntry> = self.entries.drain(..).flatten().collect();
        self.hnsw = Hnsw::<u8, DistL2>::new(
            self.params.max_nb_connection,
            10_000,
            self.params.max_layer,
            self.params.ef_construction,
            DistL2 {},
        );
        self.entries = Vec::with_capacity(live.len());
        self.gid_to_id.clear();
        for e in live {
            let id = self.entries.len();
            // e.vector 已归一化（存入即归一化）；图存 u8 量化、entries 留 f32 供精排。
            self.hnsw.insert((quantize_u8(&e.vector).as_slice(), id));
            self.gid_to_id.insert(e.gid.clone(), id);
            self.entries.push(Some(e));
        }
    }

    /// 增删后按墓碑比例触发自动压实：总槽位超下限且**墓碑过半**（dead > live）时压实。
    /// 「过半才压实」给出摊还 O(1) 重建代价（仿动态数组倍增），长跑高频 upsert/delete 下
    /// `entries`/图不再无界增长（守不变量 #6 的墓碑增长项）。
    fn maybe_compact(&mut self) {
        if self.entries.len() > COMPACT_MIN_TOTAL && self.dead_count() > self.gid_to_id.len() {
            self.compact();
        }
    }

    /// 小集合**暴力精确**扫描（保召回=1.0）：免 ANN 抖动 + 规避强过滤下召回坑。
    /// `query` 须已归一化。
    fn brute_search(
        &self,
        q: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> Vec<Scored> {
        let mut scored: Vec<Scored> = self
            .entries
            .iter()
            .flatten()
            .filter(|e| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|e| acl.is_none_or(|a| a.visible(&e.meta)))
            .map(|e| Scored {
                id: e.gid.clone(),
                score: dot(q, &e.vector) as f64,
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        scored.truncate(k);
        scored
    }

    /// 取某 gid 的引用（命中组装用）。
    pub fn citation(&self, gid: &GlobalId) -> Option<Citation> {
        self.gid_to_id
            .get(gid)
            .and_then(|&id| self.entries[id].as_ref())
            .map(|e| e.meta.citation())
    }

    /// 原子落盘：只存**向量数据**（params + 活条目的 gid/向量/meta），不存图——HNSW 图是
    /// 派生的，`load` 时由向量重建（绕开 hnsw_rs reload 的生命周期约束，契合"派生可重建"）。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let snap = HnswSnapshot {
            params: self.params,
            entries: self
                .entries
                .iter()
                .flatten()
                .map(|e| crate::SnapEntry {
                    gid: e.gid.clone(),
                    vector: e.vector.clone(),
                    meta: e.meta.clone(),
                })
                .collect(),
        };
        crate::atomic_write(path, &serde_json::to_vec(&snap)?)
    }

    /// 从快照加载并**重建图**（逐条 re-insert）。文件不存在 → 空索引（默认参数）。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)?;
        let snap: HnswSnapshot = serde_json::from_slice(&bytes)?;
        let mut idx = Self::new(snap.params);
        for e in snap.entries {
            let mut meta = e.meta;
            meta.backfill_modality(); // M4：旧快照 modality 缺省 "" → 由 kind 回填（同 MemVectorIndex::load）。
                                      // 已是归一化向量；upsert 会再次归一化（幂等：归一化向量再归一化不变）。
            idx.upsert(e.gid, e.vector, meta)?;
        }
        Ok(idx)
    }
}

/// HNSW 落盘 DTO：参数 + 活条目向量数据（图不存，load 重建）。
#[derive(Serialize, Deserialize)]
struct HnswSnapshot {
    params: HnswParams,
    entries: Vec<crate::SnapEntry>,
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
        let was_update = if let Some(old) = self.gid_to_id.remove(&gid) {
            self.entries[old] = None;
            true
        } else {
            false
        };
        let id = self.entries.len();
        let normalized = normalize(&vector);
        // 图存 u8 量化向量；entries 存 f32 供全精度重排。
        self.hnsw.insert((quantize_u8(&normalized).as_slice(), id));
        self.entries.push(Some(HnswEntry {
            gid: gid.clone(),
            vector: normalized,
            meta,
        }));
        self.gid_to_id.insert(gid, id);
        // 仅更新（产生新墓碑）时检查压实；纯新增 dead=0 永不触发，不扰 bulk load。
        if was_update {
            self.maybe_compact();
        }
        Ok(())
    }

    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        if let Some(id) = self.gid_to_id.remove(gid) {
            self.entries[id] = None;
            self.maybe_compact();
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
        self.maybe_compact();
        Ok(())
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> anyhow::Result<Vec<Scored>> {
        self.search_impl(query, k, filter, acl, None)
    }

    fn search_with_ef(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
        ef_search: Option<usize>,
    ) -> anyhow::Result<Vec<Scored>> {
        self.search_impl(query, k, filter, acl, ef_search)
    }
}

impl HnswVectorIndex {
    /// 检索实现：`ef_override` 为逐查询 `ef_search` 覆盖（None=用 `self.params.ef_search`）。
    /// 覆盖只调高/低本次遍历宽度，不改图结构——recall-vs-QPS 曲线由此一钮扫出。
    fn search_impl(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
        ef_override: Option<usize>,
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
        // 小集合回退暴力精确（保召回=1.0、确定）：免 ANN 抖动 + 强过滤召回坑。
        if self.gid_to_id.len() <= BRUTE_FALLBACK_MAX {
            return Ok(self.brute_search(&q, k, filter, acl));
        }
        // over-fetch：向 HNSW 多要候选，抵消后过滤 + u8 量化损耗（全精度重排兜底）。
        let qcode = quantize_u8(&q);
        // 升级上限用**图节点总数**（含墓碑），而非活条目数：墓碑聚簇在近邻处时，要取回足够多
        // 图节点才能筛出 k 个活条目（H5）。
        let graph_total = self.entries.len();
        let ef_base = ef_override.unwrap_or(self.params.ef_search);
        // **图内 filtered-traversal**（hnsw_rs `search_filter`）：有 filter/acl 时把 filter+ACL
        // 谓词下推进图遍历——`hnsw_filter(d_id)` 在遍历期即裁掉不合规节点，结果堆只收合规候选，
        // 强选择性下命中更密（无需"搜一大批再筛"），且遍历仍穿过被裁节点保连通性、不掉召回（守 #5）。
        // 谓词同时滤掉墓碑（`entries[id]=None`）。
        //
        // 分支选择只看 filter/acl：无 filter/acl 时走纯 `search`（零谓词开销）＋后过滤剔墓碑。
        // **不要**因"有墓碑"就改走 `search_filter`——查询恰好落在被删的相似簇里时，谓词会拒掉
        // 遍历落点附近的全部节点，导致 filtered-traversal 被"困"在簇内、返回 0（实测）。纯 `search`
        // 照常返回含墓碑的近邻，交给下方后过滤剔除、再靠自适应过取补足（H5）。
        //
        // **自适应过取（安全网）**：过滤后不足 k 就翻倍 `want` 重搜，上限=图节点总数（最坏退化全扫）。
        // 触发条件含"有墓碑"：删除一整簇相似向量后其墓碑占满 `k×over_fetch` 候选窗口，不升级则
        // 命中被墓碑吃光可返回 0（全局压实阈值挡不住局部聚簇）。确定：同输入 → 同遍历 → 同结果。
        let use_predicate = filter.is_some() || acl.is_some();
        let needs_escalation = use_predicate || self.dead_count() > 0;
        let mut want = k.saturating_mul(self.params.over_fetch).max(k);
        let mut scored: Vec<Scored> = loop {
            let ef = ef_base.max(want);
            let neighbours = if use_predicate {
                // 谓词：内部 DataId（=entries 下标）→ 活条目且过 filter+ACL 才保留。
                let pred = |id: &usize| -> bool {
                    self.entries
                        .get(*id)
                        .and_then(|e| e.as_ref())
                        .is_some_and(|e| {
                            filter.is_none_or(|f| f.eval(&e.meta))
                                && acl.is_none_or(|a| a.visible(&e.meta))
                        })
                };
                self.hnsw
                    .search_filter(qcode.as_slice(), want, ef, Some(&pred as &dyn FilterT))
            } else {
                self.hnsw.search(qcode.as_slice(), want, ef)
            };
            let s: Vec<Scored> = neighbours
                .into_iter()
                .filter_map(|n| self.entries.get(n.d_id).and_then(|e| e.as_ref()))
                // 精确后过滤：与遍历谓词同口径（防御性兜底，确保不放松/不越权）。
                .filter(|e| filter.is_none_or(|f| f.eval(&e.meta)))
                .filter(|e| acl.is_none_or(|a| a.visible(&e.meta)))
                // 全精度重排：用原始 f32 重算余弦（抵消近似/量化误差）。
                .map(|e| Scored {
                    id: e.gid.clone(),
                    score: dot(&q, &e.vector) as f64,
                })
                .collect();
            if !needs_escalation || s.len() >= k || want >= graph_total {
                break s;
            }
            want = want.saturating_mul(4).min(graph_total); // 升级过取，封顶图节点总数
        };

        // 精确安全网：HNSW 返回不足 k 时回退暴力全扫（exact）。近似遍历在病态图上可能欠交付——
        // 尤其查询落在一大簇被删的近乎相同向量里时，greedy 遍历会被"困"在墓碑 clique 内、连
        // 升级到全图也逃不出去（H5）。brute_search 线扫活条目、同口径 filter/ACL，给出真 top-k。
        // 仅在欠交付时触发（正常查询命中够 k 不进此路），代价可控。
        if scored.len() < k {
            let exact = self.brute_search(&q, k, filter, acl);
            if exact.len() > scored.len() {
                return Ok(exact);
            }
        }

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
    fn quantize_u8_boundaries() {
        // 归一化分量 [-1,1] → u8 [0,255]，对称、单调。
        assert_eq!(quantize_u8(&[-1.0, 0.0, 1.0]), vec![0u8, 128, 255]);
        let q = quantize_u8(&[-0.5, 0.5]);
        assert!(q[0] < 128 && q[1] > 128);
        // 越界裁剪（理论上归一化向量不越界，防御性）
        assert_eq!(quantize_u8(&[-2.0, 2.0]), vec![0u8, 255]);
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
        // 阈值留余量（HNSW 建图非确定，每次略有波动；50 查询平均后稳定 ≥0.9）。
        let recall = hits as f64 / (k * queries) as f64;
        assert!(recall >= 0.9, "recall@{k} = {recall} < 0.9");
    }

    #[test]
    fn no_filter_tombstone_cluster_still_returns_k() {
        // H5 回归：删除一整簇相似向量后，其墓碑仍在图中且聚簇在查询近邻处，会占满 k×over_fetch
        // 候选窗口。无 filter/acl 时旧代码 needs_escalation=false → 首轮即 break → 命中被墓碑吃光
        // 返回 0。修复后 dead_count>0 也触发自适应过取/谓词滤墓碑，取够 k 个活条目。
        let dim = 16;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        // 背景活条目 1500 条（> BRUTE_FALLBACK_MAX=1000 → 真走 HNSW），随机方向。
        for i in 0..1500u64 {
            hnsw.upsert(gid("bg", i), vec_for(i, dim), meta("bg", i, vec!["public"]))
                .unwrap();
        }
        // 查询近邻簇：600 条都贴近 q，插入后全部删除 → 查询近邻处一片墓碑。
        let q = vec_for(999_999, dim);
        for j in 0..600u64 {
            let mut v = q.clone();
            v[(j as usize) % dim] += 0.001; // 微扰，保持贴近 q 但各不相同
            hnsw.upsert(gid("cluster", j), v, meta("cluster", j, vec!["public"]))
                .unwrap();
        }
        for j in 0..600u64 {
            hnsw.delete(&gid("cluster", j)).unwrap();
        }
        assert!(hnsw.dead_count() >= 600, "簇墓碑应存在（未被自动压实）");
        assert_eq!(hnsw.len(), 1500, "活条目=背景 1500");
        // 无 filter/acl，query 落在墓碑簇附近，k=10。
        let got = hnsw.search(&q, 10, None, None).unwrap();
        assert!(
            got.len() >= 10,
            "墓碑簇不应吃光结果，应过取到活条目：got {}",
            got.len()
        );
    }

    // 逐查询 ef_search 覆盖被真正接受：① None 等同默认 search（路径不变）；
    // ② 调大 ef 召回不低于很小 ef（同索引、只转此钮——recall-vs-QPS 曲线的核心机制）。
    #[test]
    fn ef_search_override_honored() {
        let dim = 32;
        let n = 1500u64; // > BRUTE_FALLBACK_MAX → 真走 HNSW
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        let mut brute = MemVectorIndex::new();
        for i in 0..n {
            let v = vec_for(i, dim);
            hnsw.upsert(gid("d", i), v.clone(), meta("d", i, vec!["public"]))
                .unwrap();
            brute
                .upsert(gid("d", i), v, meta("d", i, vec!["public"]))
                .unwrap();
        }
        let (k, queries) = (10usize, 40u64);
        let recall_at = |ef: Option<usize>| -> f64 {
            let mut hits = 0usize;
            for qi in 0..queries {
                let q = vec_for(300_000 + qi, dim);
                let truth: std::collections::HashSet<_> = brute
                    .search(&q, k, None, None)
                    .unwrap()
                    .into_iter()
                    .map(|s| s.id)
                    .collect();
                let got = hnsw.search_with_ef(&q, k, None, None, ef).unwrap();
                hits += got.iter().filter(|s| truth.contains(&s.id)).count();
            }
            hits as f64 / (k * queries as usize) as f64
        };
        // None ⇒ 与默认 search 路径一致（用 params.ef_search）。
        let r_default = recall_at(None);
        let r_low = recall_at(Some(k)); // ef≈k，最窄遍历
        let r_high = recall_at(Some(512)); // 宽遍历
                                           // 调大 ef 不会降低召回（务实下界，避开 ANN 非确定抖动）。
        assert!(
            r_high + 1e-9 >= r_low,
            "ef 调大召回不应更差: low(ef={k})={r_low} high(ef=512)={r_high}"
        );
        assert!(
            r_default > 0.0 && r_high >= 0.9,
            "default={r_default} high={r_high}"
        );
    }

    /// 自适应过取：仅 ~1.7% 条目匹配 filter（固定 over-fetch 会被过滤殆尽 → 召回崩），
    /// 自适应升级 `want` 仍高召回（vs 暴力对 filtered 集的 ground truth）。守不变量 #5。
    #[test]
    fn adaptive_overfetch_selective_filter_recall() {
        use fastsearch_core::{FieldValue, Filter};
        let dim = 32;
        let n = 1500u64; // > BRUTE_FALLBACK_MAX → 走 HNSW 路径
        let meta_kind = |id: u64, kind: &str| {
            let mut m = meta("d", id, vec!["public"]);
            m.kind = kind.into();
            m
        };
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        let mut brute = MemVectorIndex::new();
        for i in 0..n {
            let kind = if i % 60 == 0 { "table" } else { "paragraph" }; // ~25 条 table
            let v = vec_for(i, dim);
            hnsw.upsert(gid("d", i), v.clone(), meta_kind(i, kind))
                .unwrap();
            brute.upsert(gid("d", i), v, meta_kind(i, kind)).unwrap();
        }
        let f = Filter::Eq("kind".into(), FieldValue::Str("table".into()));
        let (k, queries) = (10usize, 30u64);
        let mut hits = 0usize;
        for qi in 0..queries {
            let q = vec_for(200_000 + qi, dim);
            let truth: std::collections::HashSet<_> = brute
                .search(&q, k, Some(&f), None)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            let got = hnsw.search(&q, k, Some(&f), None).unwrap();
            hits += got.iter().filter(|s| truth.contains(&s.id)).count();
        }
        let recall = hits as f64 / (k * queries as usize) as f64;
        assert!(
            recall >= 0.85,
            "强选择性 filter recall@{k}={recall} <0.85（自适应过取应兜底）"
        );
    }

    /// 图内 filtered-traversal 的 ACL 谓词下推：在 HNSW 路径（>BRUTE_FALLBACK_MAX）上，
    /// 遍历期即裁掉越权节点 → 结果绝不含越权命中（守不变量 #3/#5），且仍有召回。
    #[test]
    fn filtered_traversal_acl_no_leak_at_scale() {
        use fastsearch_core::AclFilter;
        let dim = 32;
        let n = 1500u64; // > BRUTE_FALLBACK_MAX → 走 HNSW search_filter 路径
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        for i in 0..n {
            let acl = if i % 2 == 0 {
                vec!["team-a"]
            } else {
                vec!["team-b"]
            };
            hnsw.upsert(gid("d", i), vec_for(i, dim), meta("d", i, acl))
                .unwrap();
        }
        let acl = AclFilter {
            tenant: None,
            allowed_tags: vec!["team-a".into()],
        };
        let mut total = 0usize;
        for qi in 0..20u64 {
            let got = hnsw
                .search(&vec_for(500_000 + qi, dim), 10, None, Some(&acl))
                .unwrap();
            for s in &got {
                assert_eq!(
                    s.id.chunk_id % 2,
                    0,
                    "filtered-traversal 越权泄漏（应只见 team-a 偶数 id）"
                );
            }
            total += got.len();
        }
        assert!(total > 0, "filtered-traversal 应有合规召回，非全空");
    }

    #[test]
    fn filter_aware_acl_exact() {
        // ANN 近似 → 不保证命中 ⊆ 暴力 top-k；但**精度/ACL 必须精确**：每条结果都满足 ACL。
        use fastsearch_core::AclFilter;
        let dim = 16;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        for i in 0..400u64 {
            let v = vec_for(i, dim);
            let acl = if i % 2 == 0 {
                vec!["team-a"]
            } else {
                vec!["team-b"]
            };
            hnsw.upsert(gid("d", i), v, meta("d", i, acl)).unwrap();
        }
        let acl = AclFilter {
            tenant: None,
            allowed_tags: vec!["team-a".into()],
        };
        let q = vec_for(999, dim);
        let got = hnsw.search(&q, 10, None, Some(&acl)).unwrap();
        assert!(!got.is_empty());
        // 不越权：仅 team-a（偶数 id）可见——精确后过滤，不放松。
        for s in &got {
            assert_eq!(s.id.chunk_id % 2, 0, "越权命中（应只见 team-a 偶数 id）");
        }
    }

    #[test]
    fn save_load_roundtrip_rebuilds_graph() {
        let dim = 24;
        let n = 300u64;
        let mut idx = HnswVectorIndex::new(HnswParams::default());
        for i in 0..n {
            idx.upsert(gid("d", i), vec_for(i, dim), meta("d", i, vec!["public"]))
                .unwrap();
        }
        idx.delete(&gid("d", 5)).unwrap(); // 删一条，确保不被持久化
        let dir = std::env::temp_dir().join(format!("fs_hnsw_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hnsw.json");
        idx.save(&path).unwrap();

        let loaded = HnswVectorIndex::load(&path).unwrap();
        // 数据精确恢复：条数/维度一致、已删条目不在、活条目可解析引用。
        assert_eq!(loaded.len(), idx.len());
        assert_eq!(loaded.dim(), Some(dim));
        assert!(
            loaded.citation(&gid("d", 5)).is_none(),
            "已删条目不应持久化"
        );
        assert!(loaded.citation(&gid("d", 7)).is_some());
        // 重建图（非确定，结果可能与保存前略异）→ 只验证：返回 k 条有效命中、均在集合内、
        // 且与暴力精确高度重合（重建未损坏数据）。
        let q = vec_for(777, dim);
        let mut brute = MemVectorIndex::new();
        for i in 0..n {
            if i == 5 {
                continue;
            }
            brute
                .upsert(gid("d", i), vec_for(i, dim), meta("d", i, vec!["public"]))
                .unwrap();
        }
        let truth: std::collections::HashSet<_> = brute
            .search(&q, 10, None, None)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        let got = loaded.search(&q, 10, None, None).unwrap();
        assert_eq!(got.len(), 10);
        let overlap = got.iter().filter(|s| truth.contains(&s.id)).count();
        assert!(overlap >= 8, "load 后检索与暴力精确重合 {overlap}/10 过低");
        // 文件不存在 → 空索引
        let empty = HnswVectorIndex::load(&dir.join("nope.json")).unwrap();
        assert!(empty.is_empty());
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn auto_compaction_reclaims_tombstones() {
        let dim = 8;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        // 插入 40 条（超 COMPACT_MIN_TOTAL=32）；纯新增 → 无墓碑、不触发压实。
        for i in 1..=40u64 {
            hnsw.upsert(gid("d", i), vec_for(i, dim), meta("d", i, vec!["public"]))
                .unwrap();
        }
        assert_eq!(hnsw.entries.len(), 40, "纯新增不压实，槽位=条目数");
        assert_eq!(hnsw.dead_count(), 0);
        // 逐条删除，直到墓碑过半（dead > live）触发自动压实。
        for i in 1..=21u64 {
            hnsw.delete(&gid("d", i)).unwrap();
        }
        let live = hnsw.len();
        assert_eq!(live, 19, "应剩 19 活条目");
        // 压实已发生：槽位回落到活条目数，墓碑清零（否则槽位仍为 40）。
        assert_eq!(hnsw.dead_count(), 0, "自动压实后应无墓碑");
        assert_eq!(hnsw.entries.len(), live, "槽位回落到活条目数");
        // 活集检索语义不变：剩余 gid 仍可被自身向量召回为 top-1。
        let probe = 30u64;
        let got = hnsw.search(&vec_for(probe, dim), 1, None, None).unwrap();
        assert_eq!(got[0].id, gid("d", probe), "压实后活条目仍可检索");
    }

    #[test]
    fn manual_compact_preserves_live_set() {
        let dim = 8;
        let mut hnsw = HnswVectorIndex::new(HnswParams::default());
        for i in 1..=10u64 {
            hnsw.upsert(gid("d", i), vec_for(i, dim), meta("d", i, vec!["public"]))
                .unwrap();
        }
        // 小集合（<COMPACT_MIN_TOTAL）删除不自动压实 → 留墓碑，正好测手动 compact。
        hnsw.delete(&gid("d", 3)).unwrap();
        hnsw.delete(&gid("d", 7)).unwrap();
        assert_eq!(hnsw.dead_count(), 2, "小集合不自动压实，墓碑留存");
        let before: Vec<_> = hnsw.search(&vec_for(5, dim), 5, None, None).unwrap();
        hnsw.compact();
        assert_eq!(hnsw.dead_count(), 0, "手动压实清墓碑");
        assert_eq!(hnsw.len(), 8, "活条目数不变");
        let after: Vec<_> = hnsw.search(&vec_for(5, dim), 5, None, None).unwrap();
        // 小集合走暴力精确路径，压实前后 top-k 完全一致。
        assert_eq!(
            before.iter().map(|s| &s.id).collect::<Vec<_>>(),
            after.iter().map(|s| &s.id).collect::<Vec<_>>(),
            "压实保活集检索结果不变"
        );
        // 已删 gid 压实后仍不命中。
        assert!(after
            .iter()
            .all(|s| s.id != gid("d", 3) && s.id != gid("d", 7)));
    }
}
