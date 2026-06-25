//! # fastsearch-embed
//!
//! 嵌入后端抽象 + 离线确定性基线 + 可配置 HTTP 后端。真语义嵌入经 [`HttpEmbedder`]
//! （Ollama / OpenAI 兼容）接入——**不做进程内模型推理**。详见 [spec](../../docs/specs/16-embed.md)。
//!
//! [`HashEmbedder`] 是**确定性、零依赖**的 hashing bag-of-words 嵌入：让全链路
//! 离线/CI 可跑、可作 fallback。**非语义模型**——语义相似度需真模型。
//!
//! 真语义嵌入经**可配置 HTTP 后端**接入（[`HttpEmbedder`]）：本地 **Ollama** 或任意
//! **OpenAI 兼容** `/v1/embeddings` 端点；用 [`EmbedderConfig`]/[`build_embedder`] 选择，
//! [`EmbedderConfig::from_env`] 读环境变量。

mod http;
pub use http::{build_embedder, EmbedderConfig, EmbedderKind, HttpEmbedder, HttpProtocol};

use std::hash::{Hash, Hasher};

/// 嵌入用途（e5 风格前缀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Query,
    Passage,
}

impl EmbedKind {
    fn prefix(self) -> &'static str {
        match self {
            EmbedKind::Query => "query: ",
            EmbedKind::Passage => "passage: ",
        }
    }
}

/// 嵌入后端。
pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// 确定性、零依赖的 hashing bag-of-words 嵌入（固定维 + L2 归一化）。
///
/// 每个 token 经 hash 映射到一个维度并累加（带符号），再 L2 归一化。**非语义**，
/// 仅用于离线/CI 跑通管线与作 fallback。
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        assert!(dim > 0, "embedding dim must be > 0");
        HashEmbedder { dim }
    }

    fn embed_one(&self, text: &str, kind: EmbedKind) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        let full = format!("{}{}", kind.prefix(), text);
        for token in full
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
        {
            let token = token.to_lowercase();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            token.hash(&mut h);
            let hv = h.finish();
            let idx = (hv % self.dim as u64) as usize;
            // 符号位由 hash 另一位决定，降低碰撞抵消
            let sign = if (hv >> 32) & 1 == 0 { 1.0 } else { -1.0 };
            v[idx] += sign;
        }
        l2_normalize(&mut v);
        v
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON && norm.is_finite() {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t, kind)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_and_lengths() {
        let e = HashEmbedder::new(64);
        assert_eq!(e.dim(), 64);
        let out = e
            .embed(&["hello world".into(), "foo".into()], EmbedKind::Passage)
            .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|v| v.len() == 64));
    }

    #[test]
    fn deterministic() {
        let e = HashEmbedder::new(32);
        let a = e.embed(&["毛利率 下降".into()], EmbedKind::Query).unwrap();
        let b = e.embed(&["毛利率 下降".into()], EmbedKind::Query).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_text_and_kind_differ() {
        let e = HashEmbedder::new(128);
        let a = &e.embed(&["alpha beta".into()], EmbedKind::Query).unwrap()[0];
        let b = &e.embed(&["gamma delta".into()], EmbedKind::Query).unwrap()[0];
        assert_ne!(a, b);
        // 同文本不同 kind（前缀不同）→ 不同向量
        let q = &e.embed(&["same".into()], EmbedKind::Query).unwrap()[0];
        let p = &e.embed(&["same".into()], EmbedKind::Passage).unwrap()[0];
        assert_ne!(q, p);
    }

    #[test]
    fn l2_normalized() {
        let e = HashEmbedder::new(64);
        let v = &e
            .embed(
                &["several different tokens here".into()],
                EmbedKind::Passage,
            )
            .unwrap()[0];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn empty_text_and_batch() {
        let e = HashEmbedder::new(16);
        // 空 batch
        assert!(e.embed(&[], EmbedKind::Query).unwrap().is_empty());
        // 空字符串 → 零向量（不 panic）
        let v = &e.embed(&["".into()], EmbedKind::Query).unwrap()[0];
        assert_eq!(v.len(), 16);
        // prefix 仍会产生 token（"query"），故不一定全零；改测纯标点
        let z = &e.embed(&["!!!".into()], EmbedKind::Passage).unwrap()[0];
        assert_eq!(z.len(), 16);
    }
}
