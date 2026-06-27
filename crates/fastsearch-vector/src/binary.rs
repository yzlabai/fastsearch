//! 二值量化（1-bit）粗筛原语：RaBitQ/BQ 的核心。
//!
//! 把**归一化**向量的每维符号打成 1 bit（`v[i] >= 0 → 1`），按 u64 字打包。粗筛用
//! **Hamming 距离**（`popcount(q ^ e)`，~`d/64` 字操作 vs 精确 `d` 次浮点乘加 → 大集合快一两个
//! 数量级）做近邻预排，取 top-`k·oversample` 候选，再用**全精度 f32 重排**得精确 top-k。
//!
//! 正确性：符号 Hamming 越小 ⇒ 符号一致维越多 ⇒ 余弦越高（**粗代理**）；oversample + f32 重排把
//! 最终 top-k 拉回精确（在候选集内精确；vs 全局精确的 recall 由 oversample 决定，单测做对账）。
//! 完整 RaBitQ（随机旋转 + 无偏内积估计器）是本原语之上的精化，下一迭代。

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

/// 两个等长 bit code 的 Hamming 距离（符号不一致的维数）。
pub(crate) fn hamming(a: &[u64], b: &[u64]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_hamming_basics() {
        // 符号：[+,+,-,+] → bits 1,1,0,1
        let a = pack_signs(&[0.2, 0.0, -0.1, 0.9]);
        let b = pack_signs(&[0.2, 0.0, -0.1, 0.9]);
        assert_eq!(hamming(&a, &b), 0, "同符号 → Hamming 0");
        // 翻一个符号 → Hamming 1
        let c = pack_signs(&[0.2, 0.0, 0.1, 0.9]);
        assert_eq!(hamming(&a, &c), 1);
        // 全反 → Hamming = 维数
        let d = pack_signs(&[-0.2, -0.1, 0.1, -0.9]);
        assert_eq!(hamming(&a, &d), 4);
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
}
