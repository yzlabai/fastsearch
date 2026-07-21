//! 二值量化（1-bit）粗筛原语：RaBitQ 的核心。
//!
//! 把**归一化**向量的每维符号打成 1 bit（`v[i] >= 0 → 1`），按 u64 字打包（`code`，`ceil(d/64)`
//! 字、内存仅原 f32 的 ~1/32）。粗筛阶段不取全精度向量，只用 `code` + 查询估出近邻序，取
//! top-`k·oversample` 候选，再用**全精度 f32 重排**得精确 top-k。
//!
//! ## RaBitQ 无偏估计器（本档粗筛打分）
//! 把数据向量 `x`（单位）量化成符号向量 `x̄=sign(x)`，则其单位化为 `x̄/√d`，且 `⟨x,x̄⟩=‖x‖₁`。
//! RaBitQ 给出 `⟨q,x⟩ ≈ ⟨q,x̄⟩ / ‖x‖₁`——**用查询 `q` 的真实分量**（非对称）+ **逐向量 `‖x‖₁` 校正**。
//! 比对称 Hamming（只数符号一致维、丢掉 `q` 的幅度）更接近真实余弦：两条同符号的库向量 Hamming
//! 必打平，估计器仍能按 `q` 的幅度把它们分开 → 同 oversample 下召回更高（单测 `..._beats_hamming` 佐证）。
//!
//! 无偏性需**随机旋转**把量化误差摊匀：见下方 [`Rotation`]（数据无关正交变换，量化前施加），
//! 经 `MemVectorIndex::with_binary_prefilter_rotated` / `set_rabitq_rotation` 启用，并由 engine
//! `open_with` 据 checkpoint 翻档接线。未旋转档（默认二值）估计器已严格优于 Hamming；旋转档
//! 对各向异性数据进一步增益（单测 `rabitq_rotation_helps_anisotropic` 佐证）。重排 +
//! `GlobalId` tie-break 保持确定。

/// 把归一化向量的符号位打包成 u64 字序列（`v[i] >= 0 → 1`）。长度 `ceil(d/64)`。
pub(crate) fn pack_signs(v: &[f32]) -> Vec<u64> {
    let words = v.len().div_ceil(64);
    let mut code = vec![0u64; words];
    for (i, &x) in v.iter().enumerate() {
        if x >= 0.0 {
            code[i / 64] |= 1u64 << (i % 64);
        }
    }
    code
}

/// L1 范数 `Σ|v_i|`（= 单位向量与其符号向量的内积 `⟨x, sign(x)⟩`，RaBitQ 逐向量校正因子）。
pub(crate) fn l1_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x.abs()).sum()
}

/// RaBitQ 估计 `⟨q, x⟩ ≈ ⟨q, sign(x)⟩ / ‖x‖₁`（越大越近）。`code`=`sign(x)` 位（`bit→+1, 无→-1`），
/// `l1`=`‖x‖₁`。只读 `code`（不取全精度 x），内存轻。`l1<=0`（零向量）→ 估 0。
pub(crate) fn rabitq_estimate(q: &[f32], code: &[u64], l1: f32) -> f32 {
    if l1 <= 0.0 {
        return 0.0;
    }
    let mut s = 0.0f32;
    for (i, &qi) in q.iter().enumerate() {
        let bit = (code[i / 64] >> (i % 64)) & 1;
        if bit == 1 {
            s += qi; // sign(x_i) = +1
        } else {
            s -= qi; // sign(x_i) = -1
        }
    }
    s / l1
}

/// 固定种子的 xorshift64 → (0,1] 均匀数（确定、无 RNG 依赖；多副本/重开一致）。
fn next_unit(s: &mut u64) -> f64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    ((*s >> 11) as f64 / (1u64 << 53) as f64).max(f64::MIN_POSITIVE)
}

/// 物化 d×d 旋转矩阵的维度上限（守 DoS：不可信快照声明巨维会触发 `4·d²` 分配 + O(d³) 建矩阵）。
/// 8192 覆盖一切真实稠密嵌入模型（已知最大 4096）+2× 余量；`d>此` 标量旋转本就不可用。
/// 决策见 [governance](../../docs/governance/2026-07-21-向量旋转维度上限与DoS.md)；根治靠 FHT（下一迭代）。
pub(crate) const MAX_ROTATION_DIM: usize = 8192;

/// RaBitQ 随机正交旋转：量化前对向量做一次**数据无关**的正交变换，把信息/量化误差摊匀到各维，
/// 使符号码（`sign`）更均匀有信息、估计器趋近**无偏**——对各向异性（能量集中在少数维）数据增益尤大。
/// 由**固定种子 + 维度**确定生成（多副本/重开一致，无需持久化矩阵；正交变换不改内积，故精排仍用原向量）。
pub(crate) struct Rotation {
    dim: usize,
    mat: Vec<f32>, // d×d 行主序，行单位正交（高斯随机 + 改进 Gram-Schmidt）
}

impl Rotation {
    /// 建 d×d 旋转矩阵。`dim∈[1, MAX_ROTATION_DIM]`，越界（含 0）→ `Err`——这是**唯一分配点**，
    /// 两个旋转档（turbo / BruteBinaryRotated）都经此，DoS 闸在此一处、不可绕过。
    pub(crate) fn new(dim: usize, seed: u64) -> anyhow::Result<Self> {
        if dim == 0 || dim > MAX_ROTATION_DIM {
            anyhow::bail!("旋转维度 {dim} 越界（须 1..={MAX_ROTATION_DIM}）");
        }
        let mut s = seed | 1;
        // 高斯随机矩阵（Box–Muller）。
        let mut mat = vec![0f32; dim * dim];
        for x in mat.iter_mut() {
            let u1 = next_unit(&mut s);
            let u2 = next_unit(&mut s);
            *x = ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32;
        }
        // 改进 Gram-Schmidt：逐行减去对前行的投影 + 归一化 → 行单位正交。
        for i in 0..dim {
            for j in 0..i {
                let mut d = 0f32;
                for c in 0..dim {
                    d += mat[i * dim + c] * mat[j * dim + c];
                }
                for c in 0..dim {
                    mat[i * dim + c] -= d * mat[j * dim + c];
                }
            }
            let mut n = 0f32;
            for c in 0..dim {
                n += mat[i * dim + c] * mat[i * dim + c];
            }
            let n = n.sqrt();
            if n > f32::EPSILON {
                for c in 0..dim {
                    mat[i * dim + c] /= n;
                }
            }
        }
        Ok(Rotation { dim, mat })
    }

    /// 旋转向量：`(M·v)[i] = ⟨row_i, v⟩`。`v.len()` 须等于 `dim`。
    pub(crate) fn apply(&self, v: &[f32]) -> Vec<f32> {
        let d = self.dim;
        (0..d)
            .map(|i| {
                let row = &self.mat[i * d..(i + 1) * d];
                row.iter().zip(v).map(|(a, b)| a * b).sum()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DoS 闸：`Rotation::new` 唯一分配点拒 dim==0 / >MAX（Err 在建矩阵前立即返回，不触发巨分配）。
    /// 只测越界快路径 + 小维正常；**不**实建 8192² 矩阵（O(d³)、268MB，太慢）。
    #[test]
    fn rotation_dim_guard() {
        assert!(Rotation::new(0, 1).is_err(), "dim=0 应拒");
        assert!(
            Rotation::new(MAX_ROTATION_DIM + 1, 1).is_err(),
            "dim>MAX 应拒（不分配巨矩阵）"
        );
        assert!(Rotation::new(16, 1).is_ok(), "小维正常");
    }

    #[test]
    fn pack_signs_basics() {
        // 符号：[+,+,-,+] → bits 1,1,0,1（0.0 视为正号）
        let a = pack_signs(&[0.2, 0.0, -0.1, 0.9]);
        assert_eq!(a[0] & 0b1111, 0b1011);
    }

    #[test]
    fn pack_spans_multiple_words() {
        // 65 维 → 2 个 u64 字；第 64 位（第二字）置位。
        let mut v = vec![-1.0f32; 65];
        v[64] = 1.0;
        let code = pack_signs(&v);
        assert_eq!(code.len(), 2);
        assert_eq!(code[0], 0, "前 64 维全负 → 第一字 0");
        assert_eq!(code[1], 1, "第 64 维正 → 第二字最低位");
    }

    #[test]
    fn zero_is_positive_sign() {
        // 约定 0.0 视为正号（>=0），与 pack_signs 一致。
        assert_eq!(pack_signs(&[0.0, -0.0]), pack_signs(&[0.1, -0.0]));
    }

    #[test]
    fn estimate_separates_what_hamming_ties() {
        // q≈[0.99,0.14]（单位）。两库向量同符号 [+,+] → 对称 Hamming 必打平；
        // 估计器用 q 幅度 + 逐向量 ‖x‖₁ 校正，仍能按真实余弦把二者分开。
        let q = super::super::normalize(&[0.99, 0.14]);
        let x1 = super::super::normalize(&[1.0, 0.0]); // 真余弦更高
        let x2 = super::super::normalize(&[0.7, 0.7]);
        let (c1, c2) = (pack_signs(&x1), pack_signs(&x2));
        assert_eq!(c1, c2, "同符号 → bit code 相同（Hamming 必打平）");
        let e1 = rabitq_estimate(&q, &c1, l1_norm(&x1));
        let e2 = rabitq_estimate(&q, &c2, l1_norm(&x2));
        assert!(
            e1 > e2,
            "估计器应判 x1 更近（与真余弦序一致）：e1={e1} e2={e2}"
        );
    }

    #[test]
    fn zero_l1_estimates_zero() {
        assert_eq!(
            rabitq_estimate(&[1.0, 0.0], &pack_signs(&[0.0, 0.0]), 0.0),
            0.0
        );
    }
}
