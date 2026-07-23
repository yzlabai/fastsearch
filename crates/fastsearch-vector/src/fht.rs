//! FHT 结构化随机旋转（**唯一**旋转实现，已退役物化 d×d 矩阵）。
//!
//! 把数据无关正交旋转从**物化 d×d 矩阵**（存 O(d²)、建 O(d³)、apply O(d²)）换成
//! **随机化 Walsh-Hadamard**（存 O(d)、apply **O(d·log d)**、零建矩阵）：对输入零填充到
//! `D=next_pow2(d)`，做 `ROUNDS` 轮 `[±1 符号翻转 → 归一化 WHT]`。
//!
//! - **正交 → 保内积**：`H/√D` 与 `±1` 对角均正交 → `⟨R(q),R(v)⟩=⟨q,v⟩`（零填充不改内积），
//!   故上层量化估计器/精排语义不变。
//! - **能量摊匀**：HD 把任意方向摊到各坐标 → 符号/低比特码更有信息、坐标 ≈ N(0,1/D)，
//!   Lloyd-Max codebook 数学照旧（`quant` 标准化用 `len=D` 自动成立）。
//! - **确定**：符号由固定种子生成；WHT 确定。**无 unsafe、无新依赖**。
//!
//! 代价：非 2 幂维 `D>d` → 码宽按 D（1536→2048 +33%），换 apply ~100–200×↓、存储 O(d²)→O(d)。
//! 设计见 [plan](../../docs/plans/2026-07-22-FHT结构化旋转.md)。**Step 1：原语 + 测**（未接后端）。

/// 归一化 WHT 轮数（多轮更接近各向同性；O(D log D) 常数 ×ROUNDS）。
const ROUNDS: usize = 3;

/// 旋转维度 sanity 上限（两个旋转档 turbo/BruteBinaryRotated 共用）。**FHT 存 O(d)/apply O(d·log d)、
/// 无 d×d 矩阵**，故这不再是 DoS 关键（物化 d×d 时代才是）——纯防 `next_pow2` 溢出 / 病态巨维输入。
/// 8192 覆盖一切真实稠密嵌入（已知最大 4096）+2× 余量；真需更大改此常量即可（FHT 下代价线性）。
/// 沿革见 [governance](../../docs/governance/2026-07-21-向量旋转维度上限与DoS.md)。
pub(crate) const MAX_DIM: usize = 8192;

/// 结构化随机正交旋转。存 `ROUNDS×D` 个 ±1（O(d)）；`apply` O(d·log d)、无 d×d 矩阵。
pub(crate) struct StructuredRotation {
    padded: usize,  // D = next_pow2(d)；apply 输出维度、下游码宽/标准化按此
    signs: Vec<i8>, // ROUNDS×D 个 ±1（行主序 round*D + i）
}

/// ≥n 的最小 2 的幂（n≥1）。
fn next_pow2(n: usize) -> usize {
    let mut d = 1usize;
    while d < n {
        d <<= 1;
    }
    d
}

/// 固定种子 xorshift64 → 一个随机 bit（取高位，分布更好）。
fn next_bit(s: &mut u64) -> bool {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    (*s >> 33) & 1 == 1
}

/// 原地**未归一化** Walsh-Hadamard 变换（`a.len()` 须为 2 的幂）。O(n·log n) 蝶形。
/// `H·Hᵀ=n·I` → 未归一化时 `‖H·x‖=√n·‖x‖`（调用方乘 `1/√n` 归一）。
fn wht_inplace(a: &mut [f32]) {
    let n = a.len();
    let mut h = 1;
    while h < n {
        let mut i = 0;
        while i < n {
            for j in i..i + h {
                let x = a[j];
                let y = a[j + h];
                a[j] = x + y;
                a[j + h] = x - y;
            }
            i += 2 * h;
        }
        h <<= 1;
    }
}

impl StructuredRotation {
    /// 由 `(dim, seed)` 确定生成（多副本/重开一致）。`dim≥1`。
    pub(crate) fn new(dim: usize, seed: u64) -> Self {
        let padded = next_pow2(dim.max(1));
        let mut s = seed | 1;
        let signs: Vec<i8> = (0..ROUNDS * padded)
            .map(|_| if next_bit(&mut s) { 1i8 } else { -1i8 })
            .collect();
        StructuredRotation { padded, signs }
    }

    /// 变换后维度 `D=next_pow2(dim)`（下游码宽/标准化按此）。
    pub(crate) fn out_dim(&self) -> usize {
        self.padded
    }

    /// 旋转：零填充到 D → ROUNDS 轮 [符号翻转 + 归一化 WHT] → 返回长 D 向量。正交、保内积。
    pub(crate) fn apply(&self, v: &[f32]) -> Vec<f32> {
        let d = self.padded;
        let mut a = vec![0f32; d];
        let m = v.len().min(d);
        a[..m].copy_from_slice(&v[..m]); // 零填充（v.len() 应 == self.dim ≤ D）
        let norm = 1.0 / (d as f32).sqrt(); // 每轮 WHT 归一
        for r in 0..ROUNDS {
            let base = r * d;
            for (i, x) in a.iter_mut().enumerate() {
                *x *= self.signs[base + i] as f32;
            }
            wht_inplace(&mut a);
            for x in a.iter_mut() {
                *x *= norm;
            }
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dot, normalize};

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
        fn unit(&mut self, d: usize) -> Vec<f32> {
            normalize(&(0..d).map(|_| self.g()).collect::<Vec<_>>())
        }
    }

    /// §6.6：WHT 蝶形对拍已知 H₄ 结果（未归一化）。
    #[test]
    fn wht_known_small() {
        let mut a = [1.0f32, 0.0, 0.0, 0.0];
        wht_inplace(&mut a);
        assert_eq!(a, [1.0, 1.0, 1.0, 1.0], "H·e0 = 全 1");
        let mut b = [1.0f32, 1.0, 0.0, 0.0];
        wht_inplace(&mut b);
        assert_eq!(b, [2.0, 0.0, 2.0, 0.0]);
    }

    /// §6.1：**保内积**（正交核心）——含非 2 幂维（768/1536）。`⟨R(q),R(v)⟩ ≈ ⟨q,v⟩`。
    #[test]
    fn preserves_inner_product() {
        for d in [4usize, 8, 16, 768, 1536] {
            let mut rng = Rng(0xF17 ^ d as u64);
            let rot = StructuredRotation::new(d, 0xABCD);
            let mut max_err = 0f32;
            for _ in 0..20 {
                let q = rng.unit(d);
                let v = rng.unit(d);
                let before = dot(&q, &v);
                let after = dot(&rot.apply(&q), &rot.apply(&v));
                max_err = max_err.max((after - before).abs());
            }
            assert!(
                max_err < 5e-3,
                "d={d} 内积漂移 {max_err} 过大（应正交保内积）"
            );
        }
    }

    /// §6.2：保范数 `‖R(x)‖ ≈ ‖x‖`。
    #[test]
    fn preserves_norm() {
        for d in [16usize, 768, 2048] {
            let mut rng = Rng(0x2 ^ d as u64);
            let rot = StructuredRotation::new(d, 7);
            for _ in 0..10 {
                let x: Vec<f32> = (0..d).map(|_| rng.g()).collect();
                let n0 = dot(&x, &x).sqrt();
                let r = rot.apply(&x);
                let n1 = dot(&r, &r).sqrt();
                assert!(
                    (n1 - n0).abs() <= 1e-3 * n0.max(1.0),
                    "d={d} 范数漂移 {n0}->{n1}"
                );
            }
        }
    }

    /// §6.3：**能量摊匀**——尖峰输入 `[1,0,…]`（最集中）经旋转后无坐标独占（各向异性→均匀）。
    #[test]
    fn spreads_energy() {
        for d in [512usize, 1000] {
            let rot = StructuredRotation::new(d, 42);
            let mut spike = vec![0f32; d];
            spike[0] = 1.0;
            let r = rot.apply(&spike);
            let max_abs = r.iter().fold(0f32, |m, &x| m.max(x.abs()));
            // 单位输入 ‖r‖≈1 摊到 D 坐标 → 典型坐标 ~1/√D；断言最大坐标远小于 1（无独占）。
            assert!(max_abs < 0.3, "d={d} 最大坐标 {max_abs} 未摊匀");
        }
    }

    /// §6.4：确定性——同 (d,seed) 两次 apply 逐位一致。
    #[test]
    fn deterministic() {
        let d = 1536;
        let mut rng = Rng(0x9);
        let x = rng.unit(d);
        let a = StructuredRotation::new(d, 123);
        let b = StructuredRotation::new(d, 123);
        let (ra, rb) = (a.apply(&x), b.apply(&x));
        assert_eq!(ra.len(), rb.len());
        for (p, q) in ra.iter().zip(&rb) {
            assert_eq!(p.to_bits(), q.to_bits());
        }
    }

    /// §6.5：高维无巨分配——d=4096 apply 正常（物化档此维要 67MB+O(d³)；FHT 秒级、KB 级）。
    #[test]
    fn high_dim_no_giant_alloc() {
        let d = 4096;
        let rot = StructuredRotation::new(d, 1);
        assert_eq!(rot.out_dim(), 4096);
        let mut rng = Rng(0x5);
        let x = rng.unit(d);
        let r = rot.apply(&x);
        assert_eq!(r.len(), 4096);
        assert!((dot(&r, &r).sqrt() - 1.0).abs() < 1e-2, "保范数");
    }

    /// 非 2 幂维 → out_dim = next_pow2（下游码宽按 D）。
    #[test]
    fn out_dim_pads_to_pow2() {
        assert_eq!(StructuredRotation::new(768, 0).out_dim(), 1024);
        assert_eq!(StructuredRotation::new(1536, 0).out_dim(), 2048);
        assert_eq!(StructuredRotation::new(1024, 0).out_dim(), 1024);
        assert_eq!(StructuredRotation::new(3072, 0).out_dim(), 4096);
    }
}
