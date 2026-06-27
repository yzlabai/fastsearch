//! # fastsearch-vector
//!
//! 引擎侧向量检索后端。核心是 **filter-aware 召回**：过滤/ACL 在打分前施加，
//! 选择性强的过滤不掉召回——这正是超越 pgvector 后过滤召回崩的点（需求 §6.8）。
//!
//! v1 提供 [`MemVectorIndex`]（内存暴力余弦，精确、可测、无需模型）。HNSW + RaBitQ
//! 量化 + pgvector 直查档为下一迭代。详见 [spec](../../docs/specs/15-vector.md)。

use fastsearch_core::{
    AclFilter, BBox, Citation, FieldSource, FieldValue, Filter, GlobalId, MediaRef, Scored,
    TimeSpan,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

mod binary;
mod hnsw;
pub use hnsw::{HnswParams, HnswVectorIndex};

/// 二值量化粗筛档的默认 oversample（重开检查点时用；与 HNSW 参数同策略——格式入检查点、
/// 调参取默认）。粗筛候选数 = `k·oversample`。
pub const DEFAULT_BINARY_OVERSAMPLE: usize = 8;

/// 后端选择（engine 据此建库）。默认 `Brute`（暴力精确、确定）。
#[derive(Debug, Clone, Copy)]
pub enum VectorBackendKind {
    /// 暴力精确（默认，小/中规模、CI、需确定性）。
    Brute,
    /// 暴力 + **二值量化两阶段粗筛**（大集合更快、仍确定；on-disk 格式同 `Brute` 的 f32，
    /// 仅检索策略不同）。`usize` = oversample。
    BruteBinary(usize),
    /// HNSW 近似（大规模 opt-in；近似召回 + 非确定，见 [`HnswVectorIndex`]）。
    Hnsw(HnswParams),
}

/// 后端门面：让 engine 用单一类型持有"暴力 / HNSW"二选一，统一 upsert/search/持久化等。
/// 暴力档完全确定；HNSW 档近似+非确定（明示取舍）。
pub enum VectorStore {
    Brute(MemVectorIndex),
    Hnsw(Box<HnswVectorIndex>),
}

impl VectorStore {
    pub fn new(kind: VectorBackendKind) -> Self {
        match kind {
            VectorBackendKind::Brute => VectorStore::Brute(MemVectorIndex::new()),
            VectorBackendKind::BruteBinary(m) => {
                VectorStore::Brute(MemVectorIndex::with_binary_prefilter(m))
            }
            VectorBackendKind::Hnsw(p) => VectorStore::Hnsw(Box::new(HnswVectorIndex::new(p))),
        }
    }

    /// 后端名（落检查点，open 时据此选 loader）。二值粗筛档与暴力共享 on-disk f32 格式，但记
    /// `"brute_binary"` 以便重开时恢复粗筛档（oversample 取默认，同 HNSW 参数策略）。
    pub fn kind_str(&self) -> &'static str {
        match self {
            VectorStore::Brute(m) if m.binary_oversample().is_some() => "brute_binary",
            VectorStore::Brute(_) => "brute",
            VectorStore::Hnsw(_) => "hnsw",
        }
    }

    pub fn citation(&self, gid: &GlobalId) -> Option<Citation> {
        match self {
            VectorStore::Brute(m) => m.citation(gid),
            VectorStore::Hnsw(h) => h.citation(gid),
        }
    }

    pub fn dim(&self) -> Option<usize> {
        match self {
            VectorStore::Brute(m) => m.dim(),
            VectorStore::Hnsw(h) => h.dim(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VectorStore::Brute(m) => m.len(),
            VectorStore::Hnsw(h) => h.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        match self {
            VectorStore::Brute(m) => m.clear(),
            VectorStore::Hnsw(h) => h.clear(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        match self {
            VectorStore::Brute(m) => m.save(path),
            VectorStore::Hnsw(h) => h.save(path),
        }
    }

    /// 按后端名加载（`kind` 取自检查点；默认 brute）。文件缺失 → 空库。
    /// 二值粗筛档与暴力共享 f32 快照格式：load 同样的 f32 条目，再翻到粗筛档。
    pub fn load(kind: VectorBackendKind, path: &Path) -> anyhow::Result<Self> {
        Ok(match kind {
            VectorBackendKind::Brute => VectorStore::Brute(MemVectorIndex::load(path)?),
            VectorBackendKind::BruteBinary(m) => {
                let mut idx = MemVectorIndex::load(path)?;
                idx.set_binary_prefilter(Some(m));
                VectorStore::Brute(idx)
            }
            VectorBackendKind::Hnsw(_) => VectorStore::Hnsw(Box::new(HnswVectorIndex::load(path)?)),
        })
    }
}

impl VectorBackend for VectorStore {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()> {
        match self {
            VectorStore::Brute(m) => m.upsert(gid, vector, meta),
            VectorStore::Hnsw(h) => h.upsert(gid, vector, meta),
        }
    }
    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        match self {
            VectorStore::Brute(m) => m.delete(gid),
            VectorStore::Hnsw(h) => h.delete(gid),
        }
    }
    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        match self {
            VectorStore::Brute(m) => m.delete_doc(collection, doc_id),
            VectorStore::Hnsw(h) => h.delete_doc(collection, doc_id),
        }
    }
    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> anyhow::Result<Vec<Scored>> {
        match self {
            VectorStore::Brute(m) => m.search(query, k, filter, acl),
            VectorStore::Hnsw(h) => h.search(query, k, filter, acl),
        }
    }
}

/// 随向量存储的元数据：用于 filter/ACL 判定（实现 [`FieldSource`]）与组装引用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VecMeta {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: u64,
    pub kind: String,
    /// 模态（由 kind 派生，落列供过滤下推）。
    #[serde(default)]
    pub modality: String,
    pub page: u32,
    pub section_id: u64,
    pub heading_path: Vec<String>,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    pub bbox: BBox,
    /// 音视频时间区间（无则 None）。
    #[serde(default)]
    pub time: Option<TimeSpan>,
    /// 媒资引用（供命中组装 Citation.media；无则 None）。
    #[serde(default)]
    pub media: Option<MediaRef>,
}

impl VecMeta {
    pub fn citation(&self) -> Citation {
        Citation {
            collection: self.collection.clone(),
            doc_id: self.doc_id.clone(),
            chunk_id: self.chunk_id,
            page: self.page,
            bbox: self.bbox,
            heading_path: self.heading_path.clone(),
            section_id: self.section_id,
            time: self
                .time
                .or_else(|| self.media.as_ref().and_then(|m| m.time)),
            media: self.media.clone(),
        }
    }
}

impl FieldSource for VecMeta {
    fn get(&self, field: &str) -> Option<FieldValue> {
        match field {
            "kind" => Some(FieldValue::Str(self.kind.clone())),
            "modality" => Some(FieldValue::Str(self.modality.clone())),
            "doc_id" => Some(FieldValue::Str(self.doc_id.clone())),
            "collection" => Some(FieldValue::Str(self.collection.clone())),
            "tenant" => self.tenant.clone().map(FieldValue::Str),
            "page" => Some(FieldValue::Int(self.page as i64)),
            "section_id" => Some(FieldValue::Int(self.section_id as i64)),
            "time_start_ms" => self.time.map(|t| FieldValue::Int(t.start_ms as i64)),
            "time_end_ms" => self.time.map(|t| FieldValue::Int(t.end_ms as i64)),
            _ => None,
        }
    }
    fn heading_path(&self) -> &[String] {
        &self.heading_path
    }
    fn acl(&self) -> &[String] {
        &self.acl
    }
}

/// 向量后端抽象（trait 边界：MemVectorIndex / 未来 HNSW / pgvector 直查）。
pub trait VectorBackend {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()>;
    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()>;
    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()>;
    /// filter-aware 余弦近邻：先按 filter+acl 过滤候选，再算分取 top-k。
    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> anyhow::Result<Vec<Scored>>;
}

struct Entry {
    vector: Vec<f32>, // 归一化后存储（内积即余弦）
    code: Vec<u64>,   // 符号位 bit code（二值粗筛用，由 vector 派生）
    meta: VecMeta,
}

/// 内存暴力余弦索引（精确基线）。
///
/// 默认**精确暴力**（`binary_oversample=None`，确定）。可选 [`Self::with_binary_prefilter`] 开
/// **二值量化两阶段**：Hamming 粗筛 top-`k·oversample` → f32 精确重排（大集合更快；最终 top-k 在
/// 候选集内精确，vs 全局精确的 recall 由 oversample 决定）。重排 + GlobalId tie-break 保持确定。
#[derive(Default)]
pub struct MemVectorIndex {
    dim: Option<usize>,
    entries: HashMap<GlobalId, Entry>,
    /// None=精确暴力；Some(m)=二值粗筛取 `k·m` 候选再 f32 重排。
    binary_oversample: Option<usize>,
}

impl MemVectorIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开启二值量化两阶段粗筛（`oversample`≥1：粗筛候选数 = `k·oversample`）。
    /// `oversample` 越大召回越接近全局精确、越慢；`0` 视作 `1`。
    pub fn with_binary_prefilter(oversample: usize) -> Self {
        MemVectorIndex {
            binary_oversample: Some(oversample.max(1)),
            ..Self::default()
        }
    }

    /// 设置/关闭二值粗筛（`None`=精确暴力）。供 load 后翻档（检查点存格式、调档运行期定）。
    pub fn set_binary_prefilter(&mut self, oversample: Option<usize>) {
        self.binary_oversample = oversample.map(|m| m.max(1));
    }

    /// 当前二值粗筛 oversample（`None`=精确暴力档）。
    pub fn binary_oversample(&self) -> Option<usize> {
        self.binary_oversample
    }

    /// 取某 gid 的引用（命中组装用）。
    pub fn citation(&self, gid: &GlobalId) -> Option<Citation> {
        self.entries.get(gid).map(|e| e.meta.citation())
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 向量维度（空索引为 None）。
    pub fn dim(&self) -> Option<usize> {
        self.dim
    }

    /// 清空全部条目与维度（供单集合原地重建：坏索引→从真源重灌）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.dim = None;
    }

    /// 原子落盘：写临时文件 → fsync → rename（rename 原子，防写一半崩坏）。
    /// 存的是**已归一化**向量，load 回来 search 行为不变。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let snap = Snapshot {
            dim: self.dim,
            entries: self
                .entries
                .iter()
                .map(|(gid, e)| SnapEntry {
                    gid: gid.clone(),
                    vector: e.vector.clone(),
                    meta: e.meta.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&snap)?;
        let tmp = tmp_path(path);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?; // 落盘后再 rename，保证 rename 后内容已持久
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 从快照加载（文件不存在 → 返回空索引，便于首启）。
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let bytes = std::fs::read(path)?;
        let snap: Snapshot = serde_json::from_slice(&bytes)?;
        let mut entries = HashMap::with_capacity(snap.entries.len());
        for e in snap.entries {
            let code = binary::pack_signs(&e.vector); // 由存储的归一化向量重建（不落盘）
            entries.insert(
                e.gid,
                Entry {
                    vector: e.vector,
                    code,
                    meta: e.meta,
                },
            );
        }
        Ok(MemVectorIndex {
            dim: snap.dim,
            entries,
            binary_oversample: None, // 落盘不持搜索策略；如需开二值由调用方 with_binary_prefilter
        })
    }
}

/// 临时文件路径（同目录，便于同盘原子 rename）。
pub(crate) fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

/// 落盘快照 DTO（`HashMap<GlobalId,_>` 的 JSON key 须为字符串，故 entries 用 Vec 对）。
#[derive(Serialize, Deserialize)]
struct Snapshot {
    dim: Option<usize>,
    entries: Vec<SnapEntry>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SnapEntry {
    pub(crate) gid: GlobalId,
    pub(crate) vector: Vec<f32>,
    pub(crate) meta: VecMeta,
}

pub(crate) fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON || !norm.is_finite() {
        return vec![0.0; v.len()];
    }
    v.iter().map(|x| x / norm).collect()
}

pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl VectorBackend for MemVectorIndex {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()> {
        match self.dim {
            Some(d) if d != vector.len() => {
                anyhow::bail!("dimension mismatch: index dim {d}, got {}", vector.len())
            }
            None => self.dim = Some(vector.len()),
            _ => {}
        }
        let normalized = normalize(&vector);
        let code = binary::pack_signs(&normalized);
        self.entries.insert(
            gid,
            Entry {
                vector: normalized,
                code,
                meta,
            },
        );
        Ok(())
    }

    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        self.entries.remove(gid);
        Ok(())
    }

    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        self.entries
            .retain(|gid, _| !(gid.collection == collection && gid.doc_id == doc_id));
        Ok(())
    }

    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&Filter>,
        acl: Option<&AclFilter>,
    ) -> anyhow::Result<Vec<Scored>> {
        if let Some(d) = self.dim {
            if query.len() != d {
                anyhow::bail!("query dim {} != index dim {d}", query.len());
            }
        } else {
            return Ok(vec![]); // 空库
        }
        let q = normalize(query);

        // 真预过滤：先 filter + ACL 筛掉候选（两档共用），守不变量 #5。
        let candidates = self
            .entries
            .iter()
            .filter(|(_, e)| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|(_, e)| acl.is_none_or(|a| a.visible(&e.meta)));

        // 二值粗筛档：Hamming 取 top-(k·oversample) → 仅对候选做 f32 精确重排。
        let rerank_set: Vec<(&GlobalId, &Entry)> = if let Some(m) = self.binary_oversample {
            let qcode = binary::pack_signs(&q);
            let want = k.saturating_mul(m).max(k);
            // 粗排键：(Hamming 升, gid 升)——确定的候选集（边界同 Hamming 不抖）。
            let mut coarse: Vec<(u32, &GlobalId, &Entry)> = candidates
                .map(|(gid, e)| (binary::hamming(&qcode, &e.code), gid, e))
                .collect();
            coarse.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
            coarse.truncate(want);
            coarse.into_iter().map(|(_, gid, e)| (gid, e)).collect()
        } else {
            candidates.collect()
        };

        let mut scored: Vec<Scored> = rerank_set
            .into_iter()
            .map(|(gid, e)| Scored {
                id: gid.clone(),
                score: dot(&q, &e.vector) as f64, // f32 精确重排（两档一致）
            })
            .collect();

        // 分降序，确定性 tie-break（同分按 gid 升序）。
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

    fn gid(doc: &str, id: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
        }
    }

    fn meta(doc: &str, id: u64, kind: &str, page: u32, acl: Vec<&str>) -> VecMeta {
        VecMeta {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
            kind: kind.into(),
            modality: fastsearch_core::Modality::of_kind_str(kind)
                .as_str()
                .to_string(),
            page,
            section_id: 0,
            heading_path: vec![],
            tenant: Some("acme".into()),
            acl: acl.into_iter().map(String::from).collect(),
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            time: None,
            media: None,
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let v = idx();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vector.bin");
        v.save(&path).unwrap();
        // 临时文件应已被 rename 掉（不残留）
        assert!(!tmp_path(&path).exists());
        let loaded = MemVectorIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.dim(), Some(2));
        // 搜索结果与原索引一致（向量已归一化存储，load 不改变行为）。
        let q = vec![1.0, 0.0];
        let a = v.search(&q, 3, None, None).unwrap();
        let b = loaded.search(&q, 3, None, None).unwrap();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.id, y.id);
            assert!((x.score - y.score).abs() < 1e-9);
        }
        // 元数据/引用保留
        assert_eq!(loaded.citation(&gid("a", 3)).unwrap().page, 12);
    }

    #[test]
    fn modality_filter_pushdown() {
        let mut v = MemVectorIndex::new();
        v.upsert(
            gid("a", 1),
            vec![1.0, 0.0],
            meta("a", 1, "image", 1, vec!["public"]),
        )
        .unwrap();
        v.upsert(
            gid("a", 2),
            vec![0.0, 1.0],
            meta("a", 2, "paragraph", 1, vec!["public"]),
        )
        .unwrap();
        v.upsert(
            gid("a", 3),
            vec![0.9, 0.1],
            meta("a", 3, "audio", 1, vec!["public"]),
        )
        .unwrap();
        let q = vec![1.0, 0.0];
        // modality=image → 仅 chunk 1
        let f = Filter::Eq("modality".into(), FieldValue::Str("image".into()));
        let hits = v.search(&q, 10, Some(&f), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        // modality in {audio,video} → 仅 chunk 3
        let f2 = Filter::In(
            "modality".into(),
            vec![
                FieldValue::Str("audio".into()),
                FieldValue::Str("video".into()),
            ],
        );
        assert_eq!(v.search(&q, 10, Some(&f2), None).unwrap()[0].id.chunk_id, 3);
        // modality=text（paragraph 派生）→ 仅 chunk 2
        let f3 = Filter::Eq("modality".into(), FieldValue::Str("text".into()));
        assert_eq!(v.search(&q, 10, Some(&f3), None).unwrap()[0].id.chunk_id, 2);
    }

    #[test]
    fn time_filter_on_vecmeta() {
        let mut v = MemVectorIndex::new();
        let mut m = meta("a", 1, "audio", 1, vec!["public"]);
        m.time = Some(TimeSpan {
            start_ms: 1000,
            end_ms: 5000,
        });
        v.upsert(gid("a", 1), vec![1.0, 0.0], m).unwrap();
        v.upsert(
            gid("a", 2),
            vec![1.0, 0.0],
            meta("a", 2, "audio", 1, vec!["public"]),
        )
        .unwrap();
        // time_start_ms >= 500 → 仅有时间的 chunk 1（chunk 2 无 time → get 返回 None → 不匹配）
        let f = Filter::Gte("time_start_ms".into(), FieldValue::Int(500));
        let hits = v.search(&[1.0, 0.0], 10, Some(&f), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = MemVectorIndex::load(&dir.path().join("nope.bin")).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.dim(), None);
    }

    fn idx() -> MemVectorIndex {
        let mut v = MemVectorIndex::new();
        // 3 个 2D 向量
        v.upsert(
            gid("a", 1),
            vec![1.0, 0.0],
            meta("a", 1, "table", 5, vec!["public"]),
        )
        .unwrap();
        v.upsert(
            gid("a", 2),
            vec![0.0, 1.0],
            meta("a", 2, "paragraph", 12, vec!["public"]),
        )
        .unwrap();
        v.upsert(
            gid("a", 3),
            vec![0.9, 0.1],
            meta("a", 3, "table", 12, vec!["team-b"]),
        )
        .unwrap();
        v
    }

    #[test]
    fn cosine_ranking_topk() {
        let v = idx();
        // 查询接近 [1,0] → a1 最高，a3 次之，a2 最低
        let r = v.search(&[1.0, 0.0], 2, None, None).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].id.chunk_id, 1);
        assert_eq!(r[1].id.chunk_id, 3);
    }

    #[test]
    fn filter_aware_prefilter() {
        let v = idx();
        // kind=table → 只在 a1,a3 里排（a2 被预过滤掉，即便它余弦也参与不了）
        let f = Filter::Eq("kind".into(), FieldValue::Str("table".into()));
        let r = v.search(&[0.0, 1.0], 5, Some(&f), None).unwrap();
        assert!(r.iter().all(|s| s.id.chunk_id != 2));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn acl_blocks() {
        let v = idx();
        // 调用者 acme/team-a：a3(team-b) 不可见；a1,a2(public) 可见
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let r = v.search(&[1.0, 0.0], 5, None, Some(&acl)).unwrap();
        assert!(r.iter().all(|s| s.id.chunk_id != 3));
    }

    #[test]
    fn upsert_overwrites() {
        let mut v = idx();
        // 把 a2 改成接近 [1,0]
        v.upsert(
            gid("a", 2),
            vec![1.0, 0.0],
            meta("a", 2, "paragraph", 12, vec!["public"]),
        )
        .unwrap();
        let r = v.search(&[1.0, 0.0], 1, None, None).unwrap();
        // a1 与 a2 现在同向；tie-break 按 gid → a1(chunk 1) 在前
        assert_eq!(r[0].id.chunk_id, 1);
    }

    #[test]
    fn delete_and_delete_doc() {
        let mut v = idx();
        v.delete(&gid("a", 1)).unwrap();
        assert!(v
            .search(&[1.0, 0.0], 5, None, None)
            .unwrap()
            .iter()
            .all(|s| s.id.chunk_id != 1));
        v.delete_doc("kb", "a").unwrap();
        assert_eq!(v.search(&[1.0, 0.0], 5, None, None).unwrap().len(), 0);
    }

    #[test]
    fn dim_mismatch_and_empty() {
        let mut v = MemVectorIndex::new();
        assert_eq!(v.search(&[1.0], 5, None, None).unwrap().len(), 0); // 空库
        v.upsert(
            gid("a", 1),
            vec![1.0, 0.0],
            meta("a", 1, "table", 1, vec!["public"]),
        )
        .unwrap();
        assert!(v
            .upsert(
                gid("a", 2),
                vec![1.0],
                meta("a", 2, "table", 1, vec!["public"])
            )
            .is_err());
        assert!(v.search(&[1.0], 5, None, None).is_err()); // 维度不符
    }

    #[test]
    fn zero_vector_no_panic() {
        let mut v = MemVectorIndex::new();
        v.upsert(
            gid("a", 1),
            vec![0.0, 0.0],
            meta("a", 1, "table", 1, vec!["public"]),
        )
        .unwrap();
        let r = v.search(&[0.0, 0.0], 5, None, None).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].score, 0.0);
    }

    // ===================== 二值量化两阶段粗筛（RaBitQ/BQ 核心） =====================

    /// 确定性合成向量（xorshift，∈[-1,1)），无 RNG 依赖、可复现。
    fn pseudo_vec(seed: u64, dim: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        (0..dim)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn build(n: usize, dim: usize, oversample: Option<usize>) -> MemVectorIndex {
        let mut idx = match oversample {
            Some(m) => MemVectorIndex::with_binary_prefilter(m),
            None => MemVectorIndex::new(),
        };
        for i in 0..n as u64 {
            idx.upsert(
                gid("d", i),
                pseudo_vec(i, dim),
                meta("d", i, "paragraph", i as u32, vec!["public"]),
            )
            .unwrap();
        }
        idx
    }

    /// 强保证：oversample 大到覆盖全集 → 候选=全部 → 结果与精确暴力**逐条相同**（确定，无统计抖动）。
    #[test]
    fn binary_full_oversample_equals_exact() {
        let (n, dim, k) = (150usize, 64usize, 10usize);
        let exact = build(n, dim, None);
        let bin = build(n, dim, Some(n)); // k·n ≥ n → 覆盖全集
        let q = pseudo_vec(99_999, dim);
        let re = exact.search(&q, k, None, None).unwrap();
        let rb = bin.search(&q, k, None, None).unwrap();
        let ids_e: Vec<_> = re.iter().map(|s| s.id.chunk_id).collect();
        let ids_b: Vec<_> = rb.iter().map(|s| s.id.chunk_id).collect();
        assert_eq!(ids_e, ids_b, "全覆盖 oversample 下二值两阶段应等于精确");
        for (a, b) in re.iter().zip(&rb) {
            assert!((a.score - b.score).abs() < 1e-6, "重排分应为精确 f32");
        }
    }

    /// recall@k：中等 oversample 下，二值粗筛 + f32 重排召回应接近精确 top-k。
    #[test]
    fn binary_recall_high_with_moderate_oversample() {
        let (n, dim, k) = (300usize, 96usize, 10usize);
        let exact = build(n, dim, None);
        let bin = build(n, dim, Some(8)); // 粗筛 80 候选
        let mut hit = 0usize;
        let mut total = 0usize;
        for qseed in [1u64, 2, 3, 4, 5, 12345, 67890] {
            let q = pseudo_vec(qseed.wrapping_add(500_000), dim);
            let want: std::collections::HashSet<u64> = exact
                .search(&q, k, None, None)
                .unwrap()
                .iter()
                .map(|s| s.id.chunk_id)
                .collect();
            let got = bin.search(&q, k, None, None).unwrap();
            hit += got.iter().filter(|s| want.contains(&s.id.chunk_id)).count();
            total += k;
        }
        let recall = hit as f32 / total as f32;
        assert!(
            recall >= 0.85,
            "二值粗筛 recall@{k}={recall:.3} 应 ≥0.85（oversample=8）"
        );
    }

    /// 后端化：`VectorStore` 的 `BruteBinary` 档——`kind_str="brute_binary"`、落盘按粗筛档重载、
    /// 重载后仍粗筛 + 结果一致（检查点存格式、oversample 取默认，同 HNSW 策略）。
    #[test]
    fn vectorstore_brute_binary_roundtrip() {
        let mut s = VectorStore::new(VectorBackendKind::BruteBinary(8));
        assert_eq!(s.kind_str(), "brute_binary");
        for i in 0..40u64 {
            s.upsert(
                gid("d", i),
                pseudo_vec(i, 48),
                meta("d", i, "paragraph", i as u32, vec!["public"]),
            )
            .unwrap();
        }
        let q = pseudo_vec(777, 48);
        let before: Vec<u64> = s
            .search(&q, 6, None, None)
            .unwrap()
            .iter()
            .map(|s| s.id.chunk_id)
            .collect();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vector.bin");
        s.save(&path).unwrap();
        let reloaded = VectorStore::load(VectorBackendKind::BruteBinary(8), &path).unwrap();
        assert_eq!(reloaded.kind_str(), "brute_binary", "重载应保持粗筛档");
        let after: Vec<u64> = reloaded
            .search(&q, 6, None, None)
            .unwrap()
            .iter()
            .map(|s| s.id.chunk_id)
            .collect();
        assert_eq!(before, after, "落盘往返结果应一致");
    }

    /// 二值档仍 filter-aware：预过滤在粗筛**之前**施加，过滤外的项不进候选（守不变量 #5）。
    #[test]
    fn binary_is_filter_aware() {
        let mut bin = MemVectorIndex::with_binary_prefilter(4);
        for i in 0..50u64 {
            let kind = if i % 2 == 0 { "table" } else { "paragraph" };
            bin.upsert(
                gid("d", i),
                pseudo_vec(i, 32),
                meta("d", i, kind, i as u32, vec!["public"]),
            )
            .unwrap();
        }
        let f = Filter::Eq("kind".into(), FieldValue::Str("table".into()));
        let r = bin.search(&pseudo_vec(7, 32), 10, Some(&f), None).unwrap();
        assert!(!r.is_empty());
        assert!(
            r.iter().all(|s| s.id.chunk_id % 2 == 0),
            "二值档应只返回 kind=table（偶数 id）"
        );
    }
}
