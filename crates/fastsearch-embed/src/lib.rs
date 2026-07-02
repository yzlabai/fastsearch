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
pub use http::{
    build_embedder, EmbedderConfig, EmbedderKind, HttpEmbedder, HttpProtocol, ImageInputFormat,
};

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

/// 模态。文本（正文/caption/转录）与图像；音视频经上游转录走 `Text`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality {
    Text,
    Image,
}

/// 多模态嵌入输入（M1）：文本或图像字节。
///
/// 设计为**加法**：不动既有 [`Embedder::embed`]（纯文本路径，调用点零改），多模态走
/// [`Embedder::embed_multi`]。详见 [多模态功能设计 §4.1](../../docs/plans/2026-06-25-多模态功能设计与开发计划.md)。
#[derive(Debug, Clone)]
pub enum EmbedInput {
    Text(String),
    /// 图像原始字节（PNG/JPEG…）；后端自行 base64/送多模态端点。
    Image(Vec<u8>),
}

impl EmbedInput {
    pub fn modality(&self) -> Modality {
        match self {
            EmbedInput::Text(_) => Modality::Text,
            EmbedInput::Image(_) => Modality::Image,
        }
    }
}

/// 嵌入后端能力自描述（M1）：维度 + 支持模态 + 文图是否同一向量空间 + 是否语义模型。
///
/// `cross_modal` 是**以图搜文/以文搜图的前提**（D2：文本与图像须落同一空间）；查询期据此
/// 决定是否允许跨模态检索（否则拒绝，避免拿不可比的向量做近邻）。`semantic=false` 标记
/// 非语义后端（如 [`HashEmbedder`]：仅占位/CI，不声称语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedCaps {
    pub dim: usize,
    pub text: bool,
    pub image: bool,
    pub cross_modal: bool,
    pub semantic: bool,
}

/// 嵌入后端。
pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>>;

    /// 后端能力自描述。**默认**：纯文本、语义、非跨模态（多数文本嵌入器）。
    /// 多模态/非语义后端覆写。
    fn caps(&self) -> EmbedCaps {
        EmbedCaps {
            dim: self.dim(),
            text: true,
            image: false,
            cross_modal: false,
            semantic: true,
        }
    }

    /// 多模态嵌入（M1）。**默认实现**：仅 `Text`（委托 [`Embedder::embed`]）；遇 `Image`
    /// 报错（该后端 `caps().image=false`，需配多模态模型）。多模态后端覆写以支持图像。
    fn embed_multi(&self, inputs: &[EmbedInput], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        let texts = inputs
            .iter()
            .map(|i| match i {
                EmbedInput::Text(t) => Ok(t.clone()),
                EmbedInput::Image(_) => Err(anyhow::anyhow!(
                    "此嵌入后端不支持图像（caps.image=false）：需配多模态模型服务（M1）"
                )),
            })
            .collect::<anyhow::Result<Vec<String>>>()?;
        self.embed(&texts, kind)
    }
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

    /// 图像字节 → 确定性向量：对字节 4-gram 做特征哈希（**非语义**，仅让图像嵌入路径
    /// 离线/CI 可跑、可作 fallback）。`"img:"` 域前缀使图像向量与文本落在不同分布（HashEmbedder
    /// 本就 `cross_modal=false`，不声称文图可比）。不同图→不同向量、同图确定。
    fn embed_image_one(&self, bytes: &[u8]) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        if !bytes.is_empty() {
            let win = 4.min(bytes.len());
            for w in bytes.windows(win) {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                "img:".hash(&mut h);
                w.hash(&mut h);
                let hv = h.finish();
                let idx = (hv % self.dim as u64) as usize;
                let sign = if (hv >> 32) & 1 == 0 { 1.0 } else { -1.0 };
                v[idx] += sign;
            }
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

    /// 文图都"支持"但 **`semantic=false`、`cross_modal=false`**：图像路径仅用于离线/CI 跑通
    /// 管线（确定性占位向量），不声称语义、不声称文图同空间。真跨模态需多模态模型（M1）。
    fn caps(&self) -> EmbedCaps {
        EmbedCaps {
            dim: self.dim,
            text: true,
            image: true,
            cross_modal: false,
            semantic: false,
        }
    }

    fn embed_multi(&self, inputs: &[EmbedInput], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(inputs
            .iter()
            .map(|i| match i {
                EmbedInput::Text(t) => self.embed_one(t, kind),
                EmbedInput::Image(bytes) => self.embed_image_one(bytes),
            })
            .collect())
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

    // ============ MM8a：多模态输入 EmbedInput / caps / embed_multi ============

    #[test]
    fn hash_caps_image_nonsemantic() {
        let c = HashEmbedder::new(32).caps();
        assert!(c.text && c.image, "HashEmbedder 文图都支持（基线）");
        assert!(!c.cross_modal, "不声称文图同空间");
        assert!(!c.semantic, "非语义后端");
        assert_eq!(c.dim, 32);
    }

    #[test]
    fn embed_multi_image_deterministic_and_distinct() {
        let e = HashEmbedder::new(64);
        let img_a = EmbedInput::Image(vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4]);
        let img_b = EmbedInput::Image(vec![0xFF, 0xD8, 0xFF, 0xE0, 9, 8, 7, 6]);
        let out = e
            .embed_multi(std::slice::from_ref(&img_a), EmbedKind::Passage)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 64);
        // L2 归一化
        let norm: f32 = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        // 确定性：同图同向量
        let again = e
            .embed_multi(std::slice::from_ref(&img_a), EmbedKind::Passage)
            .unwrap();
        assert_eq!(out[0], again[0]);
        // 不同图 → 不同向量
        let b = e
            .embed_multi(std::slice::from_ref(&img_b), EmbedKind::Passage)
            .unwrap();
        assert_ne!(out[0], b[0]);
    }

    #[test]
    fn embed_multi_mixed_text_and_image() {
        let e = HashEmbedder::new(48);
        let inputs = vec![
            EmbedInput::Text("毛利率 下降".into()),
            EmbedInput::Image(vec![1, 2, 3, 4, 5]),
        ];
        let out = e.embed_multi(&inputs, EmbedKind::Passage).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|v| v.len() == 48));
        // 文本项与 embed() 文本路径一致（embed_multi(Text) 复用 embed_one）
        let text_only = e
            .embed(&["毛利率 下降".into()], EmbedKind::Passage)
            .unwrap();
        assert_eq!(out[0], text_only[0]);
    }

    #[test]
    fn empty_image_is_zero_vector() {
        let e = HashEmbedder::new(16);
        let out = e
            .embed_multi(&[EmbedInput::Image(vec![])], EmbedKind::Passage)
            .unwrap();
        assert!(
            out[0].iter().all(|&x| x == 0.0),
            "空图字节 → 零向量、不 panic"
        );
    }

    /// 纯文本后端（默认 trait 实现）：`embed_multi(Image)` 报错、`caps.image=false`。
    #[test]
    fn text_only_default_rejects_image() {
        struct TextOnly(usize);
        impl Embedder for TextOnly {
            fn dim(&self) -> usize {
                self.0
            }
            fn embed(&self, texts: &[String], _: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; self.0]).collect())
            }
        }
        let e = TextOnly(8);
        assert!(!e.caps().image, "默认 caps 不支持图像");
        // Text 走默认 embed_multi（委托 embed）→ OK
        assert!(e
            .embed_multi(&[EmbedInput::Text("x".into())], EmbedKind::Query)
            .is_ok());
        // Image → 默认实现报错
        let err = e
            .embed_multi(&[EmbedInput::Image(vec![1, 2])], EmbedKind::Query)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不支持图像"), "got: {err}");
    }
}
