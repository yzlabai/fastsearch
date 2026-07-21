//! 数据无关标量量化（TurboQuant 借鉴，arXiv 2504.19874）：把**归一化 + 随机旋转**后的
//! 单位向量按每维 `bits∈{2,3,4}` bit 压缩存储，直接在码上打分——内存降 8~16×、无训练、完全确定。
//!
//! ## 为什么能压
//! 单位向量随机正交旋转后，每维近似服从 Beta((d−1)/2,(d−1)/2)，高维时 `√d·` 该维 → **N(0,1)**。
//! 因分布已知，可**解析**（非数据训练）求 Lloyd-Max 最优量化点（[`Codebook`]）。本实现用高维高斯
//! 极限：Gaussian Lloyd-Max 的 MSE 恰为经典 Max(1960) 量化表值 {2b:0.1175, 3b:0.03454, 4b:0.009497}，
//! 与 turbovec 的精确 Beta codebook 在高维收敛到同一表——但**不引 statrs**（用初等高斯 pdf/cdf 闭式，
//! 见 [`gaussian_lloyd_max`]）。旋转由 [`crate::binary::Rotation`] 复用（不引 BLAS）。低维（d<~256）
//! 略逊，由 TQ+ 校准（下一迭代）补；目标负载是高维文本嵌入，极佳。
//!
//! ## 长度重归一化（无偏、零查询成本）
//! 标量量化系统性低估内积（重建方向略短）。编码时每向量存 `corr = 1/⟨u_rot, x̂_rot⟩`，打分时
//! `估计⟨q_unit,v_unit⟩ = ⟨q_rot, x̂_rot⟩ · corr`——正交旋转不改内积，故为无偏估计（RaBitQ 风格，
//! 是现有 1-bit `‖x‖₁` 校正的多-bit 正统版）。
//!
//! 见设计 [plan](../../docs/plans/2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md)。

/// 标准正态 pdf `φ(x)=exp(−x²/2)/√(2π)`。
fn norm_pdf(x: f64) -> f64 {
    const INV_SQRT_2PI: f64 = 0.398_942_280_401_432_7;
    INV_SQRT_2PI * (-0.5 * x * x).exp()
}

/// `erf` 数值近似（Abramowitz-Stegun 7.1.26，max err ~1.5e-7；erf 为奇函数）。
fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    y.copysign(x)
}

/// 标准正态 cdf `Φ(x)=½(1+erf(x/√2))`。
fn norm_cdf(x: f64) -> f64 {
    const INV_SQRT_2: f64 = std::f64::consts::FRAC_1_SQRT_2;
    0.5 * (1.0 + erf(x * INV_SQRT_2))
}

/// 高斯尾部有效无穷（`φ(±40)≈0`, `Φ(−40)≈0`）——最外两格用它作边界。
const BIG: f64 = 40.0;

/// 数据无关的 Gaussian Lloyd-Max 标量量化 codebook（N(0,1)，每维 `bits` bit）。
///
/// `boundaries`（`levels−1` 个，升序）= 相邻质心中点；`centroids`（`levels` 个）= 各格条件均值。
/// 由 `bits` 唯一确定（确定、无数据依赖、无外部依赖）。
pub(crate) struct Codebook {
    bits: u8,
    levels: usize,
    boundaries: Vec<f32>,
    centroids: Vec<f32>,
}

/// 单格积分量（闭式，全部由 `Φ`/`φ` 表出）：
/// `mass=∫φ=Φ(b)−Φ(a)`、`m1=∫xφ=φ(a)−φ(b)`、`m2=∫x²φ=mass−(b·φ(b)−a·φ(a))`。
fn cell_moments(a: f64, b: f64) -> (f64, f64, f64) {
    let (pa, pb) = (norm_pdf(a), norm_pdf(b));
    let mass = norm_cdf(b) - norm_cdf(a);
    let m1 = pa - pb;
    let m2 = mass - (b * pb - a * pa);
    (mass, m1, m2)
}

/// 解析求 N(0,1) 的 Lloyd-Max 量化器（`bits`∈{2,3,4}）。Lloyd 迭代：边界=质心中点、
/// 质心=格条件均值（`m1/mass`，闭式），收敛（`max_delta<tol` 或到 `MAX_ITER`）。
fn gaussian_lloyd_max(bits: u8) -> (Vec<f32>, Vec<f32>) {
    const MAX_ITER: usize = 200;
    const TOL: f64 = 1e-12;
    let levels = 1usize << bits;

    // 初值：[−3,3] 均匀撒点（Lloyd 全局收敛，初值只影响迭代数）。
    let mut c: Vec<f64> = (0..levels)
        .map(|j| -3.0 + 6.0 * (j as f64 + 0.5) / levels as f64)
        .collect();
    let mut bnd = vec![0f64; levels - 1];

    for _ in 0..MAX_ITER {
        for j in 0..levels - 1 {
            bnd[j] = 0.5 * (c[j] + c[j + 1]);
        }
        let mut max_delta = 0f64;
        for j in 0..levels {
            let a = if j == 0 { -BIG } else { bnd[j - 1] };
            let b = if j == levels - 1 { BIG } else { bnd[j] };
            let (mass, m1, _) = cell_moments(a, b);
            let nc = if mass > 1e-18 { m1 / mass } else { c[j] };
            max_delta = max_delta.max((nc - c[j]).abs());
            c[j] = nc;
        }
        if max_delta < TOL {
            break;
        }
    }
    (
        bnd.iter().map(|&x| x as f32).collect(),
        c.iter().map(|&x| x as f32).collect(),
    )
}

impl Codebook {
    /// 建 `bits` bit codebook（`bits`∈{2,3,4}；越界 clamp）。确定、无数据/外部依赖。
    pub(crate) fn new(bits: u8) -> Self {
        let bits = bits.clamp(2, 4);
        let (boundaries, centroids) = gaussian_lloyd_max(bits);
        Codebook {
            bits,
            levels: 1usize << bits,
            boundaries,
            centroids,
        }
    }

    /// 量化一个**标准化**分量（已 `×√d`）→ code = 落在第几格（`< levels`）。
    fn quantize_std(&self, s: f32) -> u8 {
        // levels≤16，线性数「小于 s 的边界数」即 code；等于边界归右格（与 Lloyd 分格一致）。
        self.boundaries.iter().take_while(|&&b| s > b).count() as u8
    }

    /// 编码一个**旋转后单位向量** `u_rot` → (位打包码, 长度重归一化 `corr`)。
    /// 标准化 `s[i]=u_rot[i]·√d` → 量化 → 打包；`corr=1/⟨u_rot, x̂_rot⟩`（`x̂_rot[i]=centroid/√d`）。
    pub(crate) fn encode(&self, u_rot: &[f32]) -> (Vec<u8>, f32) {
        let d = u_rot.len();
        let sqrt_d = (d as f32).sqrt();
        let mut codes = vec![0u8; d];
        let mut dot_uxhat = 0f32; // ⟨u_rot, x̂_rot⟩
        for (i, &u) in u_rot.iter().enumerate() {
            let code = self.quantize_std(u * sqrt_d);
            codes[i] = code;
            dot_uxhat += u * (self.centroids[code as usize] / sqrt_d);
        }
        // dot 恒 >0（码取自最近格，重建与原向量同向）；零向量/退化时兜底 corr=1。
        let corr = if dot_uxhat.abs() > f32::EPSILON {
            1.0 / dot_uxhat
        } else {
            1.0
        };
        (pack(&codes, self.bits), corr)
    }

    /// 为一个**旋转后单位查询** `q_rot` 预算查表：`lut[i*levels + c] = q_rot[i]·centroid[c]/√d`。
    /// 打分时按 code 直接查表求和（乘法只在建表时做一次；turbovec SIMD 核的标量等价）。
    pub(crate) fn query_lut(&self, q_rot: &[f32]) -> Vec<f32> {
        let d = q_rot.len();
        let inv_sqrt_d = 1.0 / (d as f32).sqrt();
        let mut lut = vec![0f32; d * self.levels];
        for (i, &q) in q_rot.iter().enumerate() {
            let base = i * self.levels;
            for c in 0..self.levels {
                lut[base + c] = q * self.centroids[c] * inv_sqrt_d;
            }
        }
        lut
    }

    /// 用预算好的 `lut` 给一个位打包码打分：`估计⟨q_unit,v_unit⟩ = (Σ_i lut[i,code_i])·corr`。
    pub(crate) fn score(&self, lut: &[f32], packed: &[u8], dim: usize, corr: f32) -> f32 {
        let mut s = 0f32;
        for i in 0..dim {
            let code = get_code(packed, self.bits, i) as usize;
            s += lut[i * self.levels + code];
        }
        s * corr
    }

    /// codebook 的量化 MSE `Σ_j(m2_j − m1_j²/mass_j)`（闭式；供单测对齐 Max 表值）。
    #[cfg(test)]
    pub(crate) fn mse(&self) -> f64 {
        let mut mse = 0f64;
        for j in 0..self.levels {
            let a = if j == 0 {
                -BIG
            } else {
                self.boundaries[j - 1] as f64
            };
            let b = if j == self.levels - 1 {
                BIG
            } else {
                self.boundaries[j] as f64
            };
            let (mass, m1, m2) = cell_moments(a, b);
            if mass > 1e-18 {
                mse += m2 - m1 * m1 / mass;
            }
        }
        mse
    }
}

/// 位打包码的字节数：`⌈dim·bits/8⌉`。
pub(crate) fn packed_len(dim: usize, bits: u8) -> usize {
    (dim * bits as usize).div_ceil(8)
}

/// LSB-first 位打包（每 code `bits` bit，可跨字节；`bits≤4` 故至多跨 2 字节）。
fn pack(codes: &[u8], bits: u8) -> Vec<u8> {
    let bits = bits as usize;
    let mut out = vec![0u8; packed_len(codes.len(), bits as u8)];
    for (idx, &c) in codes.iter().enumerate() {
        let off = idx * bits;
        let byte = off / 8;
        let sh = off % 8;
        let v = (c as u16) << sh;
        out[byte] |= (v & 0xff) as u8;
        if byte + 1 < out.len() {
            out[byte + 1] |= (v >> 8) as u8;
        }
    }
    out
}

/// 从位打包码取第 `idx` 个 code（[`pack`] 的逆，读至多 2 字节拼 u16 再移位掩码）。
fn get_code(packed: &[u8], bits: u8, idx: usize) -> u8 {
    let bits = bits as usize;
    let off = idx * bits;
    let byte = off / 8;
    let sh = off % 8;
    let lo = packed[byte] as u16;
    let hi = if byte + 1 < packed.len() {
        packed[byte + 1] as u16
    } else {
        0
    };
    let w = lo | (hi << 8);
    ((w >> sh) & ((1u16 << bits) - 1)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize;

    /// 固定种子 xorshift64（确定，无 RNG 依赖）。
    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Box-Muller 标准正态。
        fn gauss(&mut self) -> f32 {
            let u1 = self.unit().max(f64::MIN_POSITIVE);
            let u2 = self.unit();
            ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
        }
    }

    /// §8.1：Gaussian Lloyd-Max MSE 对齐经典 Max(1960) 量化表 {0.1175, 0.03454, 0.009497}（±1%，
    /// 差距主由表值 4 位有效数舍入主导——闭式 codebook 本身收敛到 1e-12）。
    #[test]
    fn codebook_mse_matches_max_table() {
        let expect = [(2u8, 0.1175f64), (3, 0.03454), (4, 0.009497)];
        for (bits, want) in expect {
            let cb = Codebook::new(bits);
            let mse = cb.mse();
            assert!(
                (mse - want).abs() / want < 0.01,
                "bits={bits} mse={mse} want~{want}"
            );
        }
    }

    /// §8.2：**重建误差不超 codebook 理论 MSE**——N(0,1) 采样量化→反量化的经验 MSE ≤ `cb.mse()·(1+ε)`
    /// （量化器就是为 N(0,1) 求 min-MSE，经验值应逼近理论值、不超之，ε 留采样噪声）。
    #[test]
    fn reconstruction_mse_within_codebook_bound() {
        for bits in [2u8, 3, 4] {
            let cb = Codebook::new(bits);
            let mut rng = Rng(0xBEEF ^ bits as u64);
            let (mut sse, mut n) = (0f64, 0f64);
            for _ in 0..100_000 {
                let s = rng.gauss(); // N(0,1)
                let code = cb.quantize_std(s) as usize;
                let recon = cb.centroids[code];
                sse += ((s - recon) as f64).powi(2);
                n += 1.0;
            }
            let empirical = sse / n;
            let theory = cb.mse();
            assert!(
                empirical <= theory * 1.03,
                "bits={bits} 经验 MSE {empirical} > 理论 {theory}·1.03"
            );
        }
    }

    /// codebook 对称（N(0,1) 对称 → 质心关于 0 反对称）。补充结构性检查。
    #[test]
    fn codebook_symmetric() {
        for bits in [2u8, 3, 4] {
            let cb = Codebook::new(bits);
            let n = cb.centroids.len();
            for j in 0..n {
                let mirror = cb.centroids[n - 1 - j];
                assert!(
                    (cb.centroids[j] + mirror).abs() < 1e-4,
                    "bits={bits} 非对称"
                );
            }
        }
    }

    /// §8.3a：位打包往返——pack→get_code 逐位还原（覆盖跨字节的 3-bit）。
    #[test]
    fn pack_roundtrip_all_widths() {
        for bits in [2u8, 3, 4] {
            let levels = 1u8 << bits;
            let mut rng = Rng(0xC0FFEE ^ bits as u64);
            let codes: Vec<u8> = (0..1000)
                .map(|_| (rng.unit() * levels as f64) as u8)
                .collect();
            let packed = pack(&codes, bits);
            assert_eq!(packed.len(), packed_len(codes.len(), bits));
            for (i, &c) in codes.iter().enumerate() {
                assert_eq!(get_code(&packed, bits, i), c, "bits={bits} idx={i}");
            }
        }
    }

    /// §8.3b：LUT 打分 == 直接 centroid 点积——encode→位打包→解包→`score` 与朴素
    /// `Σ_i q_rot[i]·centroid[code_i]/√d` 逐位一致（验证 LUT + 打包 + 打分路径无误）。
    #[test]
    fn score_equals_direct_centroid_dot() {
        for bits in [2u8, 3, 4] {
            let d = 300;
            let cb = Codebook::new(bits);
            let mut rng = Rng(0xD07 ^ bits as u64);
            let v = normalize(&(0..d).map(|_| rng.gauss()).collect::<Vec<_>>());
            let q = normalize(&(0..d).map(|_| rng.gauss()).collect::<Vec<_>>());
            let (packed, _corr) = cb.encode(&v);
            let lut = cb.query_lut(&q);
            let via_lut = cb.score(&lut, &packed, d, 1.0); // 不乘 corr，纯打分核
                                                           // 直接：解包 code → centroid 重建点积（不经 LUT）。
            let inv_sqrt_d = 1.0 / (d as f32).sqrt();
            let mut direct = 0f32;
            for (i, &qi) in q.iter().enumerate() {
                let code = get_code(&packed, bits, i) as usize;
                direct += qi * cb.centroids[code] * inv_sqrt_d;
            }
            assert!(
                (via_lut - direct).abs() <= 1e-4 * direct.abs().max(1e-3),
                "bits={bits} LUT={via_lut} != 直接={direct}"
            );
        }
    }

    /// §8.4：长度重归一化纠正内积**系统性低估**。须用**相关** q,v（真近邻、truth 显著为正）演示：
    /// 量化重建方向变短 → 不加 corr 系统性偏负；加 corr 后偏差量级显著更小。
    /// （独立随机 q,v 的 truth≈0，无低估可纠、只放大噪声——不是本校正的适用区。）
    #[test]
    fn length_renorm_reduces_bias() {
        let d = 768;
        let cb = Codebook::new(4);
        let mut rng = Rng(0x1234_5678);
        let (mut bias_corr, mut bias_raw, mut n) = (0f64, 0f64, 0f64);
        for _ in 0..300 {
            let base: Vec<f32> = (0..d).map(|_| rng.gauss()).collect();
            let v = normalize(&base);
            // q = v + 适度噪声（相关；truth≈0.8）。
            let q = normalize(
                &base
                    .iter()
                    .map(|&x| x + 0.55 * rng.gauss())
                    .collect::<Vec<_>>(),
            );
            let truth = crate::dot(&q, &v);
            let (packed, corr) = cb.encode(&v);
            let lut = cb.query_lut(&q);
            let est_corr = cb.score(&lut, &packed, d, corr);
            let est_raw = cb.score(&lut, &packed, d, 1.0); // 不加 corr
            bias_corr += (est_corr - truth) as f64;
            bias_raw += (est_raw - truth) as f64;
            n += 1.0;
        }
        let (mb_corr, mb_raw) = (bias_corr / n, bias_raw / n);
        assert!(
            mb_raw < -1e-3,
            "不加 corr 应系统性偏负（低估）: raw={mb_raw}"
        );
        assert!(
            mb_corr.abs() < mb_raw.abs(),
            "corr 未减低估偏差: |corr|={} |raw|={}",
            mb_corr.abs(),
            mb_raw.abs()
        );
    }

    /// 生成聚簇合成数据（有真实近邻结构，仿真实嵌入）：`k` 簇心，每条 = normalize(心 + σ·噪声)。
    fn clustered(rng: &mut Rng, n: usize, d: usize, k: usize, sigma: f32) -> Vec<Vec<f32>> {
        let centers: Vec<Vec<f32>> = (0..k)
            .map(|_| normalize(&(0..d).map(|_| rng.gauss()).collect::<Vec<_>>()))
            .collect();
        (0..n)
            .map(|i| {
                let c = &centers[i % k];
                let v: Vec<f32> = (0..d).map(|j| c[j] + sigma * rng.gauss()).collect();
                normalize(&v)
            })
            .collect()
    }

    fn exact_topk(q: &[f32], db: &[Vec<f32>], k: usize) -> Vec<usize> {
        let mut s: Vec<(f32, usize)> = db
            .iter()
            .enumerate()
            .map(|(i, v)| (crate::dot(q, v), i))
            .collect();
        s.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        s.into_iter().take(k).map(|(_, i)| i).collect()
    }

    /// §8.5：召回门禁——纯量化分 vs 精确暴力 ground-truth（聚簇数据，有真实近邻）。
    /// 测两个指标：
    /// - **exact@10**：量化 top-10 与精确 top-10 逐集重合（最严；密簇内相邻名次易被量化噪声互换）。
    /// - **cand@10-in-100**：精确 top-10 落在量化 top-100 内（= 量化作**候选生成器**的召回，
    ///   fastsearch"粗筛→重排"真实用法关注的指标，远高于 exact@10）。
    ///
    /// 门禁按实测留裕度设（纯量化分、无 f32 重排；真实嵌入维度更高、簇更散时召回更好）。
    #[test]
    fn recall_gate_vs_exact() {
        let d = 1024;
        let n = 2000;
        let mut rng = Rng(0x00A9_F00D);
        let db = clustered(&mut rng, n, d, 40, 0.32);
        let queries = clustered(&mut rng, 50, d, 40, 0.32);

        // (bits, exact@10 门禁, cand@10-in-100 门禁)。2-bit 粗（4 级/维）→ exact 低、是
        // **候选生成器**（配重排/oversample）；4-bit 近乎可直接当排序器。
        for (bits, gate_exact, gate_cand) in [(4u8, 0.85f32, 0.98f32), (2, 0.55, 0.90)] {
            let cb = Codebook::new(bits);
            let encoded: Vec<(Vec<u8>, f32)> = db.iter().map(|v| cb.encode(v)).collect();
            let (mut r_exact, mut r_cand) = (0f32, 0f32);
            for q in &queries {
                let truth = exact_topk(q, &db, 10);
                let lut = cb.query_lut(q);
                let mut scored: Vec<(f32, usize)> = encoded
                    .iter()
                    .enumerate()
                    .map(|(i, (packed, corr))| (cb.score(&lut, packed, d, *corr), i))
                    .collect();
                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                let ids: Vec<usize> = scored.into_iter().map(|(_, i)| i).collect();
                let top10: std::collections::HashSet<usize> =
                    ids.iter().take(10).copied().collect();
                let top100: std::collections::HashSet<usize> =
                    ids.iter().take(100).copied().collect();
                r_exact += truth.iter().filter(|i| top10.contains(i)).count() as f32 / 10.0;
                r_cand += truth.iter().filter(|i| top100.contains(i)).count() as f32 / 10.0;
            }
            r_exact /= queries.len() as f32;
            r_cand /= queries.len() as f32;
            assert!(
                r_exact >= gate_exact,
                "bits={bits} exact@10={r_exact} < gate {gate_exact}"
            );
            assert!(
                r_cand >= gate_cand,
                "bits={bits} cand@10-in-100={r_cand} < gate {gate_cand}"
            );
        }
    }

    /// §8.6：确定性——同数据两次 encode/score 逐位一致。
    #[test]
    fn deterministic() {
        let d = 256;
        let cb = Codebook::new(4);
        let mut rng = Rng(0x777);
        let v = normalize(&(0..d).map(|_| rng.gauss()).collect::<Vec<_>>());
        let q = normalize(&(0..d).map(|_| rng.gauss()).collect::<Vec<_>>());
        let (p1, c1) = cb.encode(&v);
        let (p2, c2) = cb.encode(&v);
        assert_eq!(p1, p2);
        assert_eq!(c1.to_bits(), c2.to_bits());
        let lut = cb.query_lut(&q);
        assert_eq!(
            cb.score(&lut, &p1, d, c1).to_bits(),
            cb.score(&lut, &p2, d, c2).to_bits()
        );
    }
}
