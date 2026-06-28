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
//! 无偏性需**随机旋转**把量化误差摊匀（下一迭代）；当前未旋转，估计器已严格优于 Hamming，
//! 旋转后再升级为带误差界的无偏估计。重排 + `GlobalId` tie-break 保持确定。

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

#[cfg(test)]
mod tests {
    use super::*;

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
