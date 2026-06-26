//! # fastsearch-vector
//!
//! 引擎侧向量检索后端。核心是 **filter-aware 召回**：过滤/ACL 在打分前施加，
//! 选择性强的过滤不掉召回——这正是超越 pgvector 后过滤召回崩的点（需求 §6.8）。
//!
//! v1 提供 [`MemVectorIndex`]（内存暴力余弦，精确、可测、无需模型）。HNSW + RaBitQ
//! 量化 + pgvector 直查档为下一迭代。详见 [spec](../../docs/specs/15-vector.md)。

use fastsearch_core::{
    AclFilter, BBox, Citation, FieldSource, FieldValue, Filter, GlobalId, Scored, TimeSpan,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

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
            time: self.time,
            media: None,
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
    meta: VecMeta,
}

/// 内存暴力余弦索引（精确基线）。
#[derive(Default)]
pub struct MemVectorIndex {
    dim: Option<usize>,
    entries: HashMap<GlobalId, Entry>,
}

impl MemVectorIndex {
    pub fn new() -> Self {
        Self::default()
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
            entries.insert(
                e.gid,
                Entry {
                    vector: e.vector,
                    meta: e.meta,
                },
            );
        }
        Ok(MemVectorIndex {
            dim: snap.dim,
            entries,
        })
    }
}

/// 临时文件路径（同目录，便于同盘原子 rename）。
fn tmp_path(path: &Path) -> std::path::PathBuf {
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
struct SnapEntry {
    gid: GlobalId,
    vector: Vec<f32>,
    meta: VecMeta,
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON || !norm.is_finite() {
        return vec![0.0; v.len()];
    }
    v.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
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
        self.entries.insert(
            gid,
            Entry {
                vector: normalized,
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

        let mut scored: Vec<Scored> = self
            .entries
            .iter()
            // 真预过滤：先 filter + ACL 筛掉，再进入打分集合。
            .filter(|(_, e)| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|(_, e)| acl.is_none_or(|a| a.visible(&e.meta)))
            .map(|(gid, e)| Scored {
                id: gid.clone(),
                score: dot(&q, &e.vector) as f64,
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
}
