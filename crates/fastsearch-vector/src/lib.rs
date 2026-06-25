//! # fastsearch-vector
//!
//! 引擎侧向量检索后端。核心是 **filter-aware 召回**：过滤/ACL 在打分前施加，
//! 选择性强的过滤不掉召回——这正是超越 pgvector 后过滤召回崩的点（需求 §6.8）。
//!
//! v1 提供 [`MemVectorIndex`]（内存暴力余弦，精确、可测、无需模型）。HNSW + RaBitQ
//! 量化 + pgvector 直查档为下一迭代。详见 [spec](../../docs/specs/15-vector.md)。

use fastsearch_core::{
    AclFilter, BBox, Citation, FieldSource, FieldValue, Filter, GlobalId, Scored,
};
use std::collections::HashMap;

/// 随向量存储的元数据：用于 filter/ACL 判定（实现 [`FieldSource`]）与组装引用。
#[derive(Debug, Clone)]
pub struct VecMeta {
    pub collection: String,
    pub doc_id: String,
    pub chunk_id: u64,
    pub kind: String,
    pub page: u32,
    pub section_id: u64,
    pub heading_path: Vec<String>,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    pub bbox: BBox,
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
        }
    }
}

impl FieldSource for VecMeta {
    fn get(&self, field: &str) -> Option<FieldValue> {
        match field {
            "kind" => Some(FieldValue::Str(self.kind.clone())),
            "doc_id" => Some(FieldValue::Str(self.doc_id.clone())),
            "collection" => Some(FieldValue::Str(self.collection.clone())),
            "tenant" => self.tenant.clone().map(FieldValue::Str),
            "page" => Some(FieldValue::Int(self.page as i64)),
            "section_id" => Some(FieldValue::Int(self.section_id as i64)),
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
        }
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
