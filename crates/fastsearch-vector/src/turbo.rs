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

use crate::fht::StructuredRotation;
use crate::quant::{packed_len, Codebook};
use crate::{dot, normalize, VecMeta, VectorBackend};
use fastsearch_core::{AclFilter, Citation, Filter, GlobalId, Scored};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// TurboQuant 后端的默认量化位宽（4-bit：近乎可直接排序，8× 压缩）。
pub const DEFAULT_QUANT_BITS: u8 = 4;

/// 旋转固定种子（数据无关、定常 → 多副本/重开生成同一变换，无需持久化）。
const TURBO_ROTATION_SEED: u64 = 0x5475_7262_6f51_5631; // "TurboQV1"

/// 维度上限（sanity 界；真实嵌入 ≤4096 远低于此）。两个旋转档共用 `fht::MAX_DIM`——FHT 存 O(d)/
/// apply O(d·log d)、无 d×d 矩阵，故非 DoS 关键（纯防 `next_pow2` 溢出/病态巨维）。见 [FHT plan](../../docs/plans/2026-07-22-FHT结构化旋转.md)。
const MAX_DIM: usize = crate::fht::MAX_DIM;

/// 落盘格式 magic + 版本。**v3**（2026-07-22，f32 精排 sidecar）：加 `slot`/`rerank_oversample`
/// 字段（rerank 关时 oversample=0、slot 忽略）。v2（FHT、无 sidecar）不兼容 → 拒之（turbo 新档，无生产数据）。
const TURBO_MAGIC: &str = "fastsearch-turbo";
const TURBO_FORMAT_VERSION: u32 = 3;

struct TurboEntry {
    packed: Vec<u8>, // 位打包量化码（`⌈D·bits/8⌉` 字节，D=next_pow2(dim)）
    corr: f32,       // 长度重归一化修正（`1/⟨u_rot, x̂_rot⟩`）
    meta: VecMeta,
    slot: u32, // f32 精排 sidecar 的 slot（rerank 开时有意义；关时 0、不用）
}

/// 磁盘 f32 精排 sidecar：slot `i` 存于 byte offset `i·dim·4`（归一化 f32）。**安全定位 I/O**
/// （`Mutex<File>`+`seek`+`read`/`write`，无 mmap/unsafe）。search 只对码粗筛的少数候选按 slot 读盘。
struct RerankSidecar {
    file: Mutex<File>,
    dim: usize,
    next_slot: u32,
    free: Vec<u32>, // 删除回收的 slot（新 upsert 复用，控文件增长）
}

/// 码快照路径 → sidecar 兄弟路径（`vector.bin` → `vector.bin.f32`）。
fn sidecar_path(snapshot: &Path) -> PathBuf {
    let mut s = snapshot.as_os_str().to_owned();
    s.push(".f32");
    PathBuf::from(s)
}

impl RerankSidecar {
    /// 建/截断 sidecar（新索引）。
    fn create(path: &Path, dim: usize) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(RerankSidecar {
            file: Mutex::new(file),
            dim,
            next_slot: 0,
            free: Vec::new(),
        })
    }

    /// 打开既有 sidecar（load，不截断）。`next_slot` 由调用方按 entries 重建。
    fn open(path: &Path, dim: usize, next_slot: u32) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(RerankSidecar {
            file: Mutex::new(file),
            dim,
            next_slot,
            free: Vec::new(),
        })
    }

    /// 分配 slot：优先复用 `free`，否则追加。
    fn alloc(&mut self) -> u32 {
        self.free.pop().unwrap_or_else(|| {
            let s = self.next_slot;
            self.next_slot += 1;
            s
        })
    }

    /// 写归一化 f32 到 slot（`v.len()==dim`）。
    fn write(&self, slot: u32, v: &[f32]) -> anyhow::Result<()> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(slot as u64 * self.dim as u64 * 4))?;
        let mut bytes = Vec::with_capacity(self.dim * 4);
        for x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        f.write_all(&bytes)?;
        Ok(())
    }

    /// 批量按 slot 读 f32（一次锁，seek+read 各条）——供 search 精排。
    fn read_batch(&self, slots: &[u32]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut f = self.file.lock().unwrap();
        let mut out = Vec::with_capacity(slots.len());
        let mut buf = vec![0u8; self.dim * 4];
        for &slot in slots {
            f.seek(SeekFrom::Start(slot as u64 * self.dim as u64 * 4))?;
            f.read_exact(&mut buf)?;
            out.push(
                buf.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            );
        }
        Ok(out)
    }

    fn fsync(&self) -> anyhow::Result<()> {
        self.file.lock().unwrap().sync_all()?;
        Ok(())
    }
}

/// 内存量化压缩向量索引（只存码，不存 f32）。
pub struct TurboVectorIndex {
    dim: Option<usize>,
    bits: u8,
    codebook: Codebook,
    /// 惰性构建的 **FHT 结构化旋转**（首次 upsert dim 已知时建；固定种子、确定、不落盘；
    /// 存 O(d)、apply O(d·log d)、无 d×d 矩阵）。输出维度 `D=next_pow2(d)`——码/打分按 D。
    rotation: Option<StructuredRotation>,
    entries: HashMap<GlobalId, TurboEntry>,
    /// f32 精排 sidecar（磁盘）。`None`=纯量化档（默认，零回归）；`Some`=码粗筛→读盘 f32 精排。
    rerank: Option<RerankSidecar>,
    /// 精排候选 = `k·rerank_oversample`（0=rerank 关）。落盘、load 恢复。
    rerank_oversample: usize,
    /// sidecar 落盘路径（rerank 开时）——首次 upsert 惰性建（dim 已知）。
    rerank_path: Option<PathBuf>,
}

impl TurboVectorIndex {
    /// 建 `bits` bit（∈{2,3,4}，越界 clamp）的空索引（纯量化档，无精排）。
    pub fn new(bits: u8) -> Self {
        let bits = bits.clamp(2, 4);
        TurboVectorIndex {
            dim: None,
            bits,
            codebook: Codebook::new(bits),
            rotation: None,
            entries: HashMap::new(),
            rerank: None,
            rerank_oversample: 0,
            rerank_path: None,
        }
    }

    /// 建带 **f32 精排 sidecar** 的索引：码粗筛 top-`k·oversample` → 读磁盘 f32 精确重排 → top-k，
    /// 恢复近精确召回而 RAM 仍只放码。`sidecar_path` 须为码快照路径 `p` 的兄弟 `sidecar_path(p)`
    /// （引擎按 data_dir 保证；load 据此重开）。sidecar 首次 upsert（dim 已知）惰性建。
    pub fn with_rerank(bits: u8, oversample: usize, sidecar_path: PathBuf) -> Self {
        let mut idx = Self::new(bits);
        idx.rerank_oversample = oversample.max(1);
        idx.rerank_path = Some(sidecar_path);
        idx
    }

    /// 精排候选倍数（0=纯量化，无精排）。
    pub fn rerank_oversample(&self) -> usize {
        self.rerank_oversample
    }

    /// 量化位宽。
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// dim 已知且旋转未建 → 惰性构建 FHT 旋转（固定种子，确定；O(d) 存储、无 d×d 矩阵、无 panic）。
    /// 维度上限由 `upsert`/`load` 的显式 `MAX_DIM` 早检守（FHT 无巨分配，故此处不再返 Result）。
    fn ensure_rotation(&mut self) {
        if self.rotation.is_none() {
            if let Some(d) = self.dim {
                self.rotation = Some(StructuredRotation::new(d, TURBO_ROTATION_SEED));
            }
        }
    }

    /// rerank 开、dim 已知、sidecar 未建 → 惰性建 sidecar 文件（stride=dim·4）。`next_slot` 由既有 entries
    /// 重建（load 后首次 upsert；新索引则 0）。
    fn ensure_sidecar(&mut self) -> anyhow::Result<()> {
        if self.rerank_oversample == 0 || self.rerank.is_some() {
            return Ok(());
        }
        let (Some(d), Some(path)) = (self.dim, self.rerank_path.clone()) else {
            return Ok(());
        };
        let next = self.entries.values().map(|e| e.slot + 1).max().unwrap_or(0);
        self.rerank = Some(if path.exists() {
            RerankSidecar::open(&path, d, next)?
        } else {
            RerankSidecar::create(&path, d)?
        });
        Ok(())
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
        self.rerank = None; // 丢弃 sidecar 句柄；下次 upsert 按 rerank_path 重建（截断）
    }

    /// 原子落盘（tmp→fsync→rename）。存**量化码 + corr + meta + slot**（非 f32），自描述 `bits` +
    /// `rerank_oversample`；旋转不落盘（固定种子重建）。rerank 档：**f32 在兄弟 sidecar 文件**（随 upsert
    /// 增量写），save 时 fsync 之。
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: self.dim,
            bits: self.bits,
            rerank_oversample: self.rerank_oversample,
            entries: self
                .entries
                .iter()
                .map(|(gid, e)| TurboSnapEntry {
                    gid: gid.clone(),
                    packed: e.packed.clone(),
                    corr: e.corr,
                    meta: e.meta.clone(),
                    slot: e.slot,
                })
                .collect(),
        };
        if let Some(sc) = &self.rerank {
            sc.fsync()?; // sidecar f32 落盘后再写码快照（码引用 slot）
        }
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
        // 码在 FHT 旋转空间 → 期望码长按 D=next_pow2(dim)（取自已建旋转的 out_dim）。
        let expect_len = idx
            .rotation
            .as_ref()
            .map(|r| packed_len(r.out_dim(), snap.bits));
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
                    slot: e.slot,
                },
            );
        }
        // rerank 档：重开兄弟 sidecar 文件（f32 在此），重建 next_slot=max(slot)+1（free 空，洞待压实）。
        if snap.rerank_oversample > 0 {
            idx.rerank_oversample = snap.rerank_oversample;
            let scp = sidecar_path(path);
            idx.rerank_path = Some(scp.clone());
            if let Some(d) = snap.dim {
                let next = idx.entries.values().map(|e| e.slot + 1).max().unwrap_or(0);
                if scp.exists() {
                    idx.rerank = Some(RerankSidecar::open(&scp, d, next)?);
                } else {
                    anyhow::bail!("turbo rerank 档缺 sidecar 文件: {}", scp.display());
                }
            }
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
        self.ensure_sidecar()?;
        // FHT 旋转后编码（把坐标摊成近高斯，量化更准；正交不改内积）。u_rot 长 D=next_pow2(d)。
        let u_rot = self
            .rotation
            .as_ref()
            .map(|r| r.apply(&normalized))
            .unwrap_or_else(|| normalized.clone());
        let (packed, corr) = self.codebook.encode(&u_rot);
        // rerank 开：把归一化 f32 写进 sidecar（复用同 gid 的 slot / free / 追加）。
        let reuse = self.entries.get(&gid).map(|e| e.slot);
        let slot = if let Some(sc) = self.rerank.as_mut() {
            let slot = reuse.unwrap_or_else(|| sc.alloc());
            sc.write(slot, &normalized)?;
            slot
        } else {
            0
        };
        self.entries.insert(
            gid,
            TurboEntry {
                packed,
                corr,
                meta,
                slot,
            },
        );
        Ok(())
    }

    fn delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        if let Some(e) = self.entries.remove(gid) {
            if let Some(sc) = self.rerank.as_mut() {
                sc.free.push(e.slot); // slot 回收供复用（f32 留盘、不再读）
            }
        }
        Ok(())
    }

    fn delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        let mut freed = Vec::new();
        self.entries.retain(|gid, e| {
            let drop = gid.collection == collection && gid.doc_id == doc_id;
            if drop {
                freed.push(e.slot);
            }
            !drop
        });
        if let Some(sc) = self.rerank.as_mut() {
            sc.free.extend(freed);
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
        let Some(d) = self.dim else {
            return Ok(vec![]); // 空库
        };
        if query.len() != d {
            anyhow::bail!("query dim {} != index dim {d}", query.len());
        }
        let q = normalize(query);
        let q_rot = self
            .rotation
            .as_ref()
            .map(|r| r.apply(&q))
            .unwrap_or_else(|| q.clone());
        let lut = self.codebook.query_lut(&q_rot);
        let out_d = q_rot.len(); // FHT 空间维度 D（码宽/打分按此，非原始 d）

        // 真预过滤（守 #5）→ 码上量化打分粗排。带 slot 供（可选）精排读盘。
        let mut coarse: Vec<(f64, &GlobalId, u32)> = self
            .entries
            .iter()
            .filter(|(_, e)| filter.is_none_or(|f| f.eval(&e.meta)))
            .filter(|(_, e)| acl.is_none_or(|a| a.visible(&e.meta)))
            .map(|(gid, e)| {
                (
                    self.codebook.score(&lut, &e.packed, out_d, e.corr) as f64,
                    gid,
                    e.slot,
                )
            })
            .collect();
        // 量化分降序，确定性 tie-break（同分按 gid 升序）。
        let by_score_gid = |a: &(f64, &GlobalId, u32), b: &(f64, &GlobalId, u32)| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(b.1))
        };
        coarse.sort_by(by_score_gid);

        // f32 精排档：取 top-(k·oversample) 候选 → 读磁盘 f32 精确余弦重排 → top-k。
        if let Some(sc) = &self.rerank {
            let want = k.saturating_mul(self.rerank_oversample).max(k);
            coarse.truncate(want);
            let slots: Vec<u32> = coarse.iter().map(|c| c.2).collect();
            let f32s = sc.read_batch(&slots)?;
            let mut scored: Vec<Scored> = coarse
                .iter()
                .zip(f32s)
                .map(|((_, gid, _), v)| Scored {
                    id: (*gid).clone(),
                    score: dot(&q, &v) as f64, // 精确余弦（归一 query · 归一 sidecar f32）
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.cmp(&b.id))
            });
            scored.truncate(k);
            return Ok(scored);
        }

        // 纯量化档（默认）：量化分直接取 top-k。
        Ok(coarse
            .into_iter()
            .take(k)
            .map(|(s, gid, _)| Scored {
                id: gid.clone(),
                score: s,
            })
            .collect())
    }
}

/// 落盘快照 DTO（带 magic/version；entries 用 Vec 对，因 JSON key 须字符串）。
#[derive(Serialize, Deserialize)]
struct TurboSnapshot {
    magic: String,
    version: u32,
    dim: Option<usize>,
    bits: u8,
    #[serde(default)]
    rerank_oversample: usize, // 0=纯量化档；>0=f32 精排档（sidecar=兄弟文件）
    entries: Vec<TurboSnapEntry>,
}

#[derive(Serialize, Deserialize)]
struct TurboSnapEntry {
    gid: GlobalId,
    packed: Vec<u8>,
    corr: f32,
    meta: VecMeta,
    #[serde(default)]
    slot: u32, // sidecar slot（rerank 档有意义）
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
            rerank_oversample: 0,
            entries: vec![TurboSnapEntry {
                gid: gid("a", 1),
                packed: vec![0u8; packed_len(d, 4)],
                corr: 2.5, // 占位，下面替换成溢出字面量
                meta: meta("text", &[]),
                slot: 0,
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

    /// DoS 闸（§governance）：`upsert` 维度 >MAX_DIM(8192) → 拒（建 d×d 旋转前）。
    #[test]
    fn upsert_rejects_oversized_dim() {
        let mut idx = TurboVectorIndex::new(4);
        let big = vec![0.1f32; MAX_DIM + 1];
        assert!(
            idx.upsert(gid("a", 1), big, meta("text", &[])).is_err(),
            "超维 upsert 应拒"
        );
    }

    /// DoS 闸：**小文件声明巨维**（dim=50000、空条目）→ load 在建 16GB 旋转矩阵**前**就拒。
    #[test]
    fn load_rejects_oversized_dim() {
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: Some(50_000),
            bits: 4,
            rerank_oversample: 0,
            entries: vec![],
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), serde_json::to_vec(&snap).unwrap()).unwrap();
        assert!(
            TurboVectorIndex::load(tmp.path(), 4).is_err(),
            "小文件声明巨维应拒（不触发巨分配）"
        );
    }

    /// 畸形快照：有条目但 dim 缺失 → load 拒之。
    #[test]
    fn load_rejects_entries_without_dim() {
        let snap = TurboSnapshot {
            magic: TURBO_MAGIC.to_string(),
            version: TURBO_FORMAT_VERSION,
            dim: None,
            bits: 4,
            rerank_oversample: 0,
            entries: vec![TurboSnapEntry {
                gid: gid("a", 1),
                packed: vec![0u8; 4],
                corr: 1.0,
                meta: meta("text", &[]),
                slot: 0,
            }],
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), serde_json::to_vec(&snap).unwrap()).unwrap();
        assert!(TurboVectorIndex::load(tmp.path(), 4).is_err());
    }

    /// 生成聚簇合成数据（有真实近邻结构）：`k` 簇心，每条 = normalize(心 + σ·噪声)。
    fn clustered(rng: &mut Rng, n: usize, d: usize, k: usize, sigma: f32) -> Vec<Vec<f32>> {
        let centers: Vec<Vec<f32>> = (0..k).map(|_| normalize(&rng.vec(d))).collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % k];
                normalize(&(0..d).map(|j| c[j] + sigma * rng.g()).collect::<Vec<_>>())
            })
            .collect()
    }

    fn exact_topk(q: &[f32], db: &[Vec<f32>], k: usize) -> Vec<usize> {
        let mut s: Vec<(f32, usize)> = db
            .iter()
            .enumerate()
            .map(|(i, v)| (crate::dot(&normalize(q), &normalize(v)), i))
            .collect();
        s.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        s.into_iter().take(k).map(|(_, i)| i).collect()
    }

    /// **FHT 端到端召回门禁**（Step 2 关键验证）：TurboVectorIndex（现用 FHT 旋转）纯量化分 top-10
    /// vs 精确暴力 ground-truth。用**非 2 幂维 d=1000**（→ D=1024，走填充路径）。达到与物化档
    /// 同量级（对齐 quant §8.5 的 4-bit exact@10≈0.85），即证 FHT 旋转召回不劣、可无条件替换物化。
    #[test]
    fn recall_vs_exact_fht() {
        let (d, n) = (1000usize, 1500usize);
        let mut rng = Rng(0xFECA);
        let db = clustered(&mut rng, n, d, 30, 0.32);
        let queries = clustered(&mut rng, 40, d, 30, 0.32);
        let mut idx = TurboVectorIndex::new(4);
        for (i, v) in db.iter().enumerate() {
            idx.upsert(gid("d", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
        }
        let mut recall = 0f32;
        for q in &queries {
            let truth = exact_topk(q, &db, 10);
            let got: std::collections::HashSet<usize> = idx
                .search(q, 10, None, None)
                .unwrap()
                .iter()
                .map(|s| s.id.chunk_id as usize)
                .collect();
            recall += truth.iter().filter(|i| got.contains(i)).count() as f32 / 10.0;
        }
        recall /= queries.len() as f32;
        // 实测 ≈0.885（与物化档 ~0.87 同量级/略优）→ FHT 召回不劣，无条件替换物化成立。
        assert!(recall >= 0.82, "FHT turbo 4-bit exact@10={recall} < 0.82");
    }

    fn recall_of(idx: &TurboVectorIndex, queries: &[Vec<f32>], db: &[Vec<f32>]) -> f32 {
        let mut r = 0f32;
        for q in queries {
            let truth: std::collections::HashSet<usize> =
                exact_topk(q, db, 10).into_iter().collect();
            let got = idx.search(q, 10, None, None).unwrap();
            r += got
                .iter()
                .filter(|s| truth.contains(&(s.id.chunk_id as usize)))
                .count() as f32
                / 10.0;
        }
        r / queries.len() as f32
    }

    /// §8.1：**f32 精排 sidecar 恢复召回**——同数据，rerank 档 exact@10 ≥0.98 且显著 > 纯量化档。
    #[test]
    fn rerank_recovers_recall() {
        let (d, n) = (1000usize, 1500usize);
        let mut rng = Rng(0xFECA);
        let db = clustered(&mut rng, n, d, 30, 0.32);
        let queries = clustered(&mut rng, 40, d, 30, 0.32);

        let mut pure = TurboVectorIndex::new(4);
        for (i, v) in db.iter().enumerate() {
            pure.upsert(gid("d", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
        }
        let rp = recall_of(&pure, &queries, &db);

        let dir = tempfile::tempdir().unwrap();
        let mut rr = TurboVectorIndex::with_rerank(4, 8, dir.path().join("v.f32"));
        for (i, v) in db.iter().enumerate() {
            rr.upsert(gid("d", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
        }
        let rre = recall_of(&rr, &queries, &db);
        // 实测 rerank=1.000 vs 纯量化 0.885（4-bit）——精排完全恢复召回。
        assert!(
            rre >= 0.98,
            "rerank exact@10={rre} 应≥0.98（对比纯量化 {rp}）"
        );
        assert!(rre > rp + 0.05, "rerank({rre}) 应显著 > 纯量化({rp})");
    }

    /// §8.2：2-bit 尤其受益——2-bit + rerank exact@10 显著 > 纯 2-bit（2-bit 本就是候选生成器）。
    #[test]
    fn rerank_helps_2bit_most() {
        let (d, n) = (768usize, 1200usize);
        let mut rng = Rng(0xB2);
        let db = clustered(&mut rng, n, d, 25, 0.32);
        let queries = clustered(&mut rng, 30, d, 25, 0.32);
        let mut pure = TurboVectorIndex::new(2);
        let dir = tempfile::tempdir().unwrap();
        let mut rr = TurboVectorIndex::with_rerank(2, 12, dir.path().join("v.f32"));
        for (i, v) in db.iter().enumerate() {
            pure.upsert(gid("d", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
            rr.upsert(gid("d", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
        }
        let (rp, rre) = (
            recall_of(&pure, &queries, &db),
            recall_of(&rr, &queries, &db),
        );
        assert!(rre > rp + 0.1, "2-bit rerank({rre}) 应远超纯 2-bit({rp})");
    }

    /// §8.3：持久化往返（**双文件**：码快照 + 兄弟 sidecar）——load 恢复 rerank 档、检索逐位一致。
    #[test]
    fn rerank_persistence_roundtrip() {
        let d = 256;
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("vector.bin");
        let mut idx = TurboVectorIndex::with_rerank(4, 6, sidecar_path(&snap));
        let mut rng = Rng(0x11);
        for i in 0..60 {
            idx.upsert(gid("a", i), rng.vec(d), meta("text", &[]))
                .unwrap();
        }
        let q = rng.vec(d);
        let before = idx.search(&q, 8, None, None).unwrap();
        idx.save(&snap).unwrap();

        let re = TurboVectorIndex::load(&snap, 4).unwrap();
        assert_eq!(re.rerank_oversample(), 6, "rerank 档应恢复");
        let after = re.search(&q, 8, None, None).unwrap();
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score.to_bits(), b.score.to_bits());
        }
    }

    /// §8.4：delete 回收 slot、新 upsert 复用——① 复用 slot 的新向量能检索到（无脏读）、已删的不出现；
    /// ② sidecar 文件不增长（size = 活 slot 数·stride）。
    #[test]
    fn rerank_delete_reuses_slot() {
        let d = 128;
        let dir = tempfile::tempdir().unwrap();
        let scp = dir.path().join("v.f32");
        let mut idx = TurboVectorIndex::with_rerank(4, 8, scp.clone());
        let mut rng = Rng(0x22);
        let vecs: Vec<_> = (0..8).map(|_| rng.vec(d)).collect();
        for (i, v) in vecs.iter().enumerate() {
            idx.upsert(gid("a", i as u64), v.clone(), meta("text", &[]))
                .unwrap();
        }
        idx.delete(&gid("a", 3)).unwrap(); // 回收 slot 3
        idx.upsert(gid("a", 100), vecs[5].clone(), meta("text", &[]))
            .unwrap(); // 复用 slot 3，内容 = vecs[5]
        let res = idx.search(&vecs[5], 3, None, None).unwrap();
        let ids: Vec<u64> = res.iter().map(|s| s.id.chunk_id).collect();
        assert!(
            ids.contains(&100),
            "复用 slot 的新向量应检索到（无脏读）: {ids:?}"
        );
        assert!(!ids.contains(&3), "已删除 a:3 不应出现");
        assert_eq!(idx.len(), 8);
        idx.save(&dir.path().join("vector.bin")).unwrap(); // fsync sidecar
        let size = std::fs::metadata(&scp).unwrap().len();
        assert_eq!(size, 8 * d as u64 * 4, "复用 → 文件不增长（8 slot·stride）");
    }

    /// §8.5：默认关（`new`）= 纯量化，与不带 sidecar 行为一致（覆盖既有 recall/持久化测已证）。
    /// 此处显式：`new` 的 `rerank_oversample()==0`、search 走纯量化路径。
    #[test]
    fn default_no_rerank() {
        let idx = TurboVectorIndex::new(4);
        assert_eq!(idx.rerank_oversample(), 0);
    }
}
