//! TurboQuant 压缩主索引后端（借鉴 turbovec / Google TurboQuant）。
//!
//! 与 [`crate::MemVectorIndex`]（f32 主存 + 可选二值粗筛）不同，本后端**只存 2–4bit 量化码 +
//! 每向量一个 f32 修正标量**，f32 根本不存 → 内存降 8~16×；直接在码上打分（[`crate::quant`]）。
//!
//! - **无训练**：codebook 由维度/bit 解析确定（守不变量 #2：派生可重建）。
//! - **完全确定**：旋转固定种子、codebook 确定、同分按 `GlobalId` 升序 tie-break（守 #4）——
//!   这是相对 HNSW（非确定）的关键卖点。
//! - **filter-aware**：打分前 `Filter::eval`+`AclFilter::visible` 真预过滤（守 #5）。
//! - **诚实**：纯量化分近似召回（4-bit exact@10≈0.87 / 候选@100≥0.98；2-bit 需重排），见
//!   [`crate::quant`] 门禁与 [spec 15](../../docs/specs/15-vector.md)。
//!
//! 设计见 [plan](../../docs/plans/2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md)。

use crate::binary::Rotation;
use crate::quant::{packed_len, Codebook};
use crate::{normalize, VecMeta, VectorBackend};
use fastsearch_core::{AclFilter, Citation, Filter, GlobalId, Scored};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// TurboQuant 后端的默认量化位宽（4-bit：近乎可直接排序，8× 压缩）。
pub const DEFAULT_QUANT_BITS: u8 = 4;

/// 旋转矩阵固定种子（数据无关、定常 → 多副本/重开生成同一矩阵，无需持久化）。
const TURBO_ROTATION_SEED: u64 = 0x5475_7262_6f51_5631; // "TurboQV1"

/// 维度上限（防不可信快照声明巨维触发 d×d 旋转矩阵内存爆炸；借 turbovec `MAX_DIM`）。
const MAX_DIM: usize = 65536;

/// 落盘格式 magic + 版本（带版本起步，便于将来平滑升级；借 turbovec io.rs）。
const TURBO_MAGIC: &str = "fastsearch-turbo";
const TURBO_FORMAT_VERSION: u32 = 1;

struct TurboEntry {
    packed: Vec<u8>, // 位打包量化码（`⌈dim·bits/8⌉` 字节）
    corr: f32,       // 长度重归一化修正（`1/⟨u_rot, x̂_rot⟩`）
    meta: VecMeta,
}

/// 内存量化压缩向量索引（只存码，不存 f32）。
pub struct TurboVectorIndex {
    dim: Option<usize>,
    bits: u8,
    codebook: Codebook,
    /// 惰性构建的旋转矩阵（首次 upsert dim 已知时建；固定种子、确定、不落盘）。
    rotation: Option<Rotation>,
    entries: HashMap<GlobalId, TurboEntry>,
}

impl TurboVectorIndex {
    /// 建 `bits` bit（∈{2,3,4}，越界 clamp）的空索引。
    pub fn new(bits: u8) -> Self {
        let bits = bits.clamp(2, 4);
        TurboVectorIndex {
            dim: None,
            bits,
            codebook: Codebook::new(bits),
            rotation: None,
            entries: HashMap::new(),
        }
    }

    /// 量化位宽。
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// dim 已知且矩阵未建 → 惰性构建（固定种子，确定）。
    fn ensure_rotation(&mut self) {
        if self.rotation.is_none() {
            if let Some(d) = self.dim {
                self.rotation = Some(Rotation::new(d, TURBO_ROTATION_SEED));
            }
        }
    }

    pub fn citation(&self, gid: &GlobalId) -> Option<Citation> {
        self.entries.get(gid).map(|e| e.meta.citation())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn dim(&self) -> Option<usize> {
        self.dim
    }

    /// 清空全部条目与维度（供单集合原地重建：坏索引→从真源重灌）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.dim = None;
        self.rotation = None;
    }

    /// 原子落盘（tmp→fsync→rename）。存**量化码 + corr + meta**（非 f32），自描述 `bits`；
    /// 旋转矩阵不落盘（固定种子，load 重建）。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: self.dim,
            bits: self.bits,
            entries: self
                .entries
                .iter()
                .map(|(gid, e)| TurboSnapEntry {
                    gid: gid.clone(),
                    packed: e.packed.clone(),
                    corr: e.corr,
                    meta: e.meta.clone(),
                })
                .collect(),
        };
        crate::atomic_write(path, &serde_json::to_vec(&snap)?)
    }

    /// 从快照加载（文件不存在 → 空索引，用 `default_bits`）。`bits` 由文件自描述（存盘时定的位宽
    /// 决定码的解码，不能由调用方覆盖）；旋转矩阵由固定种子重建。校验 magic/version/bits/dim。
    pub fn load(path: &Path, default_bits: u8) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::new(default_bits));
        }
        let bytes = std::fs::read(path)?;
        let snap: TurboSnapshot = serde_json::from_slice(&bytes)?;
        if snap.magic != TURBO_MAGIC {
            anyhow::bail!("turbo 快照 magic 不符: {:?}", snap.magic);
        }
        if snap.version != TURBO_FORMAT_VERSION {
            anyhow::bail!(
                "turbo 快照版本 {} 不支持（本版 {TURBO_FORMAT_VERSION}）",
                snap.version
            );
        }
        if !(2..=4).contains(&snap.bits) {
            anyhow::bail!("turbo 快照 bits={} 非法（须 2..=4）", snap.bits);
        }
        if let Some(d) = snap.dim {
            if d == 0 || d > MAX_DIM {
                anyhow::bail!("turbo 快照 dim={d} 越界（1..={MAX_DIM}）");
            }
        } else if !snap.entries.is_empty() {
            anyhow::bail!("turbo 快照有条目但 dim 缺失（畸形快照）");
        }
        let mut idx = TurboVectorIndex::new(snap.bits);
        idx.dim = snap.dim;
        idx.ensure_rotation();
        let expect_len = snap.dim.map(|d| packed_len(d, snap.bits));
        for e in snap.entries {
            if let Some(el) = expect_len {
                if e.packed.len() != el {
                    anyhow::bail!("turbo 快照码长 {} != 期望 {el}", e.packed.len());
                }
            }
            // 毒丸防护：不可信快照的 corr 若非有限（Inf/NaN）→ 打分被毒化，拒之（守 DoS 护栏 §7）。
            if !e.corr.is_finite() {
                anyhow::bail!("turbo 快照 corr={} 非有限（畸形/毒丸快照）", e.corr);
            }
            let mut meta = e.meta;
            meta.backfill_modality();
            idx.entries.insert(
                e.gid,
                TurboEntry {
                    packed: e.packed,
                    corr: e.corr,
                    meta,
                },
            );
        }
        Ok(idx)
    }
}

impl VectorBackend for TurboVectorIndex {
    fn upsert(&mut self, gid: GlobalId, vector: Vec<f32>, meta: VecMeta) -> anyhow::Result<()> {
        match self.dim {
            Some(d) if d != vector.len() => {
                anyhow::bail!("dimension mismatch: index dim {d}, got {}", vector.len())
            }
            None => {
                if vector.is_empty() || vector.len() > MAX_DIM {
                    anyhow::bail!("向量维度 {} 越界（1..={MAX_DIM}）", vector.len());
                }
                self.dim = Some(vector.len());
            }
            _ => {}
        }
        let normalized = normalize(&vector); // 零/NaN → 全零（不 panic）
        self.ensure_rotation();
        // 旋转后编码（旋转把坐标摊成近高斯，量化更准）；正交不改内积。
        let u_rot = self
            .rotation
            .as_ref()
            .map(|r| r.apply(&normalized))
            .unwrap_or(normalized);
        let (packed, corr) = self.codebook.encode(&u_rot);
        self.entries.insert(gid, TurboEntry { packed, corr, meta });
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
        let Some(d) = self.dim else {
            return Ok(vec![]); // 空库
        };
        if query.len() != d {
            anyhow::bail!("query dim {} != index dim {d}", query.len());
        }
        let q = normalize(query);
        let q_rot = self.rotation.as_ref().map(|r| r.apply(&q)).unwrap_or(q);
        let lut = self.codebook.query_lut(&q_rot);

        // 真预过滤：先 filter + ACL 筛候选，再码上打分（守不变量 #5）。
        let mut scored: Vec<Scored> = self
            .entries
            .iter()
            .filter(|(_, e)| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|(_, e)| acl.is_none_or(|a| a.visible(&e.meta)))
            .map(|(gid, e)| Scored {
                id: gid.clone(),
                score: self.codebook.score(&lut, &e.packed, d, e.corr) as f64,
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

/// 落盘快照 DTO（带 magic/version；entries 用 Vec 对，因 JSON key 须字符串）。
#[derive(Serialize, Deserialize)]
struct TurboSnapshot {
    magic: String,
    version: u32,
    dim: Option<usize>,
    bits: u8,
    entries: Vec<TurboSnapEntry>,
}

#[derive(Serialize, Deserialize)]
struct TurboSnapEntry {
    gid: GlobalId,
    packed: Vec<u8>,
    corr: f32,
    meta: VecMeta,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, FieldValue};

    fn gid(doc: &str, id: u64) -> GlobalId {
        GlobalId {
            collection: "kb".into(),
            doc_id: doc.into(),
            chunk_id: id,
        }
    }

    fn meta(kind: &str, acl: &[&str]) -> VecMeta {
        VecMeta {
            collection: "kb".into(),
            doc_id: "d".into(),
            chunk_id: 0,
            kind: kind.into(),
            modality: fastsearch_core::Modality::of_kind_str(kind)
                .as_str()
                .to_string(),
            page: 0,
            section_id: 0,
            heading_path: vec![],
            tenant: None,
            acl: acl.iter().map(|s| s.to_string()).collect(),
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

    /// 固定种子 xorshift → Box-Muller 高斯（确定）。
    struct Rng(u64);
    impl Rng {
        fn g(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let u1 = ((self.0 >> 11) as f64 / (1u64 << 53) as f64).max(f64::MIN_POSITIVE);
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let u2 = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
        }
        fn vec(&mut self, d: usize) -> Vec<f32> {
            (0..d).map(|_| self.g()).collect()
        }
    }

    /// 检索：query 贴近某条目 → 该条目排第一；top-k 截断；只存码不存 f32。
    #[test]
    fn search_ranks_nearest_first() {
        let d = 256;
        let mut rng = Rng(0x11);
        let mut idx = TurboVectorIndex::new(4);
        let target = rng.vec(d);
        idx.upsert(gid("a", 1), target.clone(), meta("text", &[]))
            .unwrap();
        for i in 2..20 {
            idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                .unwrap();
        }
        // query = target + 小噪声 → 仍最近。
        let q: Vec<f32> = target.iter().map(|&x| x + 0.05 * rng.g()).collect();
        let res = idx.search(&q, 5, None, None).unwrap();
        assert_eq!(res.len(), 5);
        assert_eq!(res[0].id, gid("a", 1), "最近条目应排第一");
    }

    /// filter-aware：kind 过滤后只在匹配项里排（真预过滤）。
    #[test]
    fn filter_aware_prefilter() {
        let d = 128;
        let mut rng = Rng(0x22);
        let mut idx = TurboVectorIndex::new(4);
        let q = rng.vec(d);
        idx.upsert(gid("a", 1), q.clone(), meta("table", &[]))
            .unwrap(); // 最近但 kind=table
        for i in 2..10 {
            idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                .unwrap();
        }
        let f = Filter::Eq("kind".into(), FieldValue::Str("text".into()));
        let res = idx.search(&q, 10, Some(&f), None).unwrap();
        assert!(res.iter().all(|s| s.id != gid("a", 1)), "table 项被过滤");
        assert!(!res.is_empty());
    }

    /// ACL：越权项不出现在结果（不泄漏）。
    #[test]
    fn acl_no_leak() {
        let d = 64;
        let mut rng = Rng(0x33);
        let mut idx = TurboVectorIndex::new(4);
        let q = rng.vec(d);
        idx.upsert(gid("a", 1), q.clone(), meta("text", &["secret"]))
            .unwrap();
        idx.upsert(gid("a", 2), rng.vec(d), meta("text", &["public"]))
            .unwrap();
        let acl = AclFilter {
            tenant: None,
            allowed_tags: vec!["public".into()],
        };
        let res = idx.search(&q, 10, None, Some(&acl)).unwrap();
        assert!(res.iter().all(|s| s.id != gid("a", 1)), "无权项泄漏");
    }

    /// upsert 覆盖 + delete + delete_doc。
    #[test]
    fn upsert_overwrite_and_delete() {
        let d = 32;
        let mut rng = Rng(0x44);
        let mut idx = TurboVectorIndex::new(4);
        idx.upsert(gid("a", 1), rng.vec(d), meta("text", &[]))
            .unwrap();
        idx.upsert(gid("a", 1), rng.vec(d), meta("text", &[]))
            .unwrap(); // 覆盖
        assert_eq!(idx.len(), 1);
        idx.upsert(gid("b", 2), rng.vec(d), meta("text", &[]))
            .unwrap();
        idx.delete(&gid("a", 1)).unwrap();
        assert_eq!(idx.len(), 1);
        idx.upsert(gid("b", 3), rng.vec(d), meta("text", &[]))
            .unwrap();
        idx.delete_doc("kb", "b").unwrap();
        assert_eq!(idx.len(), 0);
    }

    /// 维度不匹配报错；空库空结果；零向量不 panic。
    #[test]
    fn dim_mismatch_empty_zero() {
        let mut idx = TurboVectorIndex::new(4);
        // 空库
        assert!(idx.search(&[1.0, 2.0], 5, None, None).unwrap().is_empty());
        idx.upsert(gid("a", 1), vec![1.0, 0.0, 0.0], meta("text", &[]))
            .unwrap();
        // 维度不匹配
        assert!(idx
            .upsert(gid("a", 2), vec![1.0, 0.0], meta("text", &[]))
            .is_err());
        assert!(idx.search(&[1.0, 0.0], 5, None, None).is_err());
        // 零向量不 panic
        idx.upsert(gid("a", 3), vec![0.0, 0.0, 0.0], meta("text", &[]))
            .unwrap();
        assert_eq!(
            idx.search(&[1.0, 0.0, 0.0], 5, None, None).unwrap().len(),
            2
        );
    }

    /// 确定性：同输入两次建库 + 检索，结果逐条一致（含同分 gid tie-break）。
    #[test]
    fn deterministic() {
        let d = 128;
        let build = || {
            let mut rng = Rng(0x55);
            let mut idx = TurboVectorIndex::new(4);
            for i in 0..30 {
                idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                    .unwrap();
            }
            idx
        };
        let q = Rng(0x99).vec(d);
        let r1 = build().search(&q, 10, None, None).unwrap();
        let r2 = build().search(&q, 10, None, None).unwrap();
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(&r2) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }

    /// 持久化往返：save→load 后检索结果一致、bits 自描述恢复、内存=码字节数（不存 f32）。
    #[test]
    fn persistence_roundtrip() {
        let d = 256;
        let mut rng = Rng(0x66);
        let mut idx = TurboVectorIndex::new(2); // 用 2-bit 验证 bits 自描述
        for i in 0..50 {
            idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                .unwrap();
        }
        let q = rng.vec(d);
        let before = idx.search(&q, 10, None, None).unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        idx.save(tmp.path()).unwrap();
        // 内存红利：每条码字节 = ⌈d·bits/8⌉（2-bit → d/4）。
        assert_eq!(packed_len(d, 2), d / 4);

        // load 用 default_bits=4，但文件自描述 bits=2 → 恢复 2-bit。
        let re = TurboVectorIndex::load(tmp.path(), 4).unwrap();
        assert_eq!(re.bits(), 2, "bits 应由文件自描述恢复");
        assert_eq!(re.len(), 50);
        let after = re.search(&q, 10, None, None).unwrap();
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }

    /// 缺文件 → 空库（用 default_bits）。
    #[test]
    fn load_missing_is_empty() {
        let idx = TurboVectorIndex::load(Path::new("/no/such/turbo.bin"), 3).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.bits(), 3);
    }

    /// bits=3 档持久化往返（3-bit 位打包跨字节，与 2/4-bit 一并守）。
    #[test]
    fn roundtrip_bits3() {
        let d = 192;
        let mut rng = Rng(0x77);
        let mut idx = TurboVectorIndex::new(3);
        for i in 0..40 {
            idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                .unwrap();
        }
        let q = rng.vec(d);
        let before = idx.search(&q, 8, None, None).unwrap();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        idx.save(tmp.path()).unwrap();
        let re = TurboVectorIndex::load(tmp.path(), 4).unwrap();
        assert_eq!(re.bits(), 3, "3-bit 由文件自描述恢复");
        let after = re.search(&q, 8, None, None).unwrap();
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }

    /// DoS 护栏（§7）：不可信快照的 `corr` 非有限 → 打分被毒化 → `load` 拒之。JSON 无 Infinity
    /// 字面量，真实攻击向量是**溢出 f32 但合法 f64 的字面量**（`1e39` > f32 max → serde `as f32`
    /// 静默得 Inf、不报错），故手工改文本注入以触达 `corr.is_finite()` 守卫。
    #[test]
    fn load_rejects_poisoned_corr() {
        let d = 8;
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: Some(d),
            bits: 4,
            entries: vec![TurboSnapEntry {
                gid: gid("a", 1),
                packed: vec![0u8; packed_len(d, 4)],
                corr: 2.5, // 占位，下面替换成溢出字面量
                meta: meta("text", &[]),
            }],
        };
        let text = serde_json::to_string(&snap)
            .unwrap()
            .replace("\"corr\":2.5", "\"corr\":1e39"); // 1e39 > f32 max → serde 静默得 Inf
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), text).unwrap();
        let err = match TurboVectorIndex::load(tmp.path(), 4) {
            Ok(_) => panic!("应因 corr 非有限拒绝加载"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("corr"), "应因 corr 非有限拒之: {err}");
    }

    /// 畸形快照：有条目但 dim 缺失 → load 拒之。
    #[test]
    fn load_rejects_entries_without_dim() {
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: None,
            bits: 4,
            entries: vec![TurboSnapEntry {
                gid: gid("a", 1),
                packed: vec![0u8; 4],
                corr: 1.0,
                meta: meta("text", &[]),
            }],
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), serde_json::to_vec(&snap).unwrap()).unwrap();
        assert!(TurboVectorIndex::load(tmp.path(), 4).is_err());
    }
}
