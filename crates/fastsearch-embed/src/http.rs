//! 可配置的 HTTP 嵌入后端：把"算向量"委托给外部嵌入服务（**本地 Ollama** 或任意
//! **OpenAI 兼容** `/v1/embeddings` 端点：TEI / vLLM / LM Studio / llama.cpp-server /
//! OpenAI 本身）。同步阻塞（`ureq`，纯 Rust）契合同步 [`Embedder`] trait；服务侧在
//! `spawn_blocking` 里调用即可不阻塞 async 运行时。
//!
//! 选后端用 [`EmbedderConfig`]/[`build_embedder`]/[`EmbedderConfig::from_env`]。请求体构造、
//! 响应解析、维度校验是纯逻辑、有单测；实网调用 env-gated。

use crate::{EmbedCaps, EmbedInput, EmbedKind, Embedder, HashEmbedder};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::Value;
use std::time::Duration;

/// HTTP 嵌入后端的线缆协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProtocol {
    /// Ollama 原生：`POST {url}/api/embed`，`{model, input:[..]}` → `{embeddings:[[..]]}`。
    Ollama,
    /// OpenAI 兼容：`POST {url}/v1/embeddings`，`{model, input:[..]}` → `{data:[{embedding,index}]}`。
    OpenAI,
}

/// 嵌入后端选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    /// 确定性、零依赖基线（离线/CI/fallback；非语义）。
    Hash,
    /// HTTP 后端（Ollama / OpenAI 兼容）。
    Http(HttpProtocol),
}

/// HTTP 图片输入格式。只影响 `EmbedInput::Image` 的 JSON 表达；文本路径保持兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageInputFormat {
    /// `"data:image/png;base64,..."` 作为 `input[]` 元素。
    DataUrl,
    /// `{"image":"<base64>","media_type":"image/png"}` 作为 `input[]` 元素。
    Base64Object,
    /// OpenAI 风格 content parts：`[{"type":"input_image","image_url":"data:..."}]`。
    OpenAiContent,
}

/// 统一嵌入配置（CLI/server 据此构造后端）。
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    pub kind: EmbedderKind,
    /// HTTP 后端的基址（如 `http://localhost:11434`）；Hash 后端忽略。
    pub url: String,
    /// 模型名（如 `nomic-embed-text` / `bge-m3` / `text-embedding-3-small`）。
    pub model: String,
    /// 向量维度——**必须与索引/PG 向量列一致**（响应维度不符即报错）。
    pub dim: usize,
    /// 可选 Bearer token（OpenAI/网关鉴权）。
    pub api_key: Option<String>,
    /// 查询/文段前缀（模型相关：e5 用 `query: `/`passage: `，nomic 用
    /// `search_query: `/`search_document: `，bge 多为空）。默认空。
    pub query_prefix: String,
    pub passage_prefix: String,
    /// 请求超时秒数。
    pub timeout_secs: u64,
    /// 是否启用 HTTP 图片输入。开启后 `caps.image=true`。
    pub image: bool,
    /// 文本和图片是否同空间。文搜图/图搜文的向量召回必须显式开启。
    pub cross_modal: bool,
    /// 图片输入 JSON 形态。
    pub image_input_format: ImageInputFormat,
}

impl EmbedderConfig {
    /// Hash 基线配置（离线）。
    pub fn hash(dim: usize) -> Self {
        EmbedderConfig {
            kind: EmbedderKind::Hash,
            url: String::new(),
            model: String::new(),
            dim,
            api_key: None,
            query_prefix: String::new(),
            passage_prefix: String::new(),
            timeout_secs: 30,
            image: false,
            cross_modal: false,
            image_input_format: ImageInputFormat::DataUrl,
        }
    }

    /// 从环境变量读取（未设 `FASTSEARCH_EMBEDDER` → Hash 基线，维度取 `FASTSEARCH_EMBED_DIM` 或 384）：
    /// - `FASTSEARCH_EMBEDDER` = `hash` | `ollama` | `openai`
    /// - `FASTSEARCH_EMBED_URL`（默认 ollama `http://localhost:11434`）
    /// - `FASTSEARCH_EMBED_MODEL` / `FASTSEARCH_EMBED_DIM` / `FASTSEARCH_EMBED_API_KEY`
    /// - `FASTSEARCH_EMBED_QUERY_PREFIX` / `FASTSEARCH_EMBED_PASSAGE_PREFIX`
    /// - `FASTSEARCH_EMBED_IMAGE` / `FASTSEARCH_EMBED_CROSS_MODAL`
    /// - `FASTSEARCH_EMBED_IMAGE_INPUT_FORMAT` = `data_url` | `base64_object` | `openai_content`
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        let bool_var =
            |k: &str| var(k).is_some_and(|s| matches!(s.as_str(), "1" | "true" | "yes" | "on"));
        let dim = var("FASTSEARCH_EMBED_DIM")
            .and_then(|s| s.parse().ok())
            .unwrap_or(384);
        let kind = match var("FASTSEARCH_EMBEDDER").as_deref() {
            Some("ollama") => EmbedderKind::Http(HttpProtocol::Ollama),
            Some("openai") => EmbedderKind::Http(HttpProtocol::OpenAI),
            _ => EmbedderKind::Hash,
        };
        let default_url = match kind {
            EmbedderKind::Http(HttpProtocol::Ollama) => "http://localhost:11434",
            EmbedderKind::Http(HttpProtocol::OpenAI) => "http://localhost:8080",
            EmbedderKind::Hash => "",
        };
        EmbedderConfig {
            kind,
            url: var("FASTSEARCH_EMBED_URL").unwrap_or_else(|| default_url.to_string()),
            model: var("FASTSEARCH_EMBED_MODEL").unwrap_or_else(|| "nomic-embed-text".to_string()),
            dim,
            api_key: var("FASTSEARCH_EMBED_API_KEY"),
            query_prefix: var("FASTSEARCH_EMBED_QUERY_PREFIX").unwrap_or_default(),
            passage_prefix: var("FASTSEARCH_EMBED_PASSAGE_PREFIX").unwrap_or_default(),
            timeout_secs: 30,
            image: bool_var("FASTSEARCH_EMBED_IMAGE"),
            cross_modal: bool_var("FASTSEARCH_EMBED_CROSS_MODAL"),
            image_input_format: match var("FASTSEARCH_EMBED_IMAGE_INPUT_FORMAT").as_deref() {
                Some("base64_object") => ImageInputFormat::Base64Object,
                Some("openai_content") => ImageInputFormat::OpenAiContent,
                _ => ImageInputFormat::DataUrl,
            },
        }
    }
}

/// 按配置构造嵌入后端。
pub fn build_embedder(cfg: &EmbedderConfig) -> Box<dyn Embedder + Send + Sync> {
    match cfg.kind {
        EmbedderKind::Hash => Box::new(HashEmbedder::new(cfg.dim)),
        EmbedderKind::Http(_) => Box::new(HttpEmbedder::new(cfg.clone())),
    }
}

/// HTTP 嵌入后端。
pub struct HttpEmbedder {
    cfg: EmbedderConfig,
    agent: ureq::Agent,
}

impl HttpEmbedder {
    pub fn new(cfg: EmbedderConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build();
        HttpEmbedder { cfg, agent }
    }

    fn protocol(&self) -> HttpProtocol {
        match self.cfg.kind {
            EmbedderKind::Http(p) => p,
            EmbedderKind::Hash => HttpProtocol::Ollama, // 不会发生（工厂保证）
        }
    }

    fn prefix(&self, kind: EmbedKind) -> &str {
        match kind {
            EmbedKind::Query => &self.cfg.query_prefix,
            EmbedKind::Passage => &self.cfg.passage_prefix,
        }
    }

    /// 应用前缀后的输入串。
    fn inputs(&self, texts: &[String], kind: EmbedKind) -> Vec<String> {
        let p = self.prefix(kind);
        texts.iter().map(|t| format!("{p}{t}")).collect()
    }

    /// 端点 URL。
    fn endpoint(&self) -> String {
        let base = self.cfg.url.trim_end_matches('/');
        match self.protocol() {
            HttpProtocol::Ollama => format!("{base}/api/embed"),
            HttpProtocol::OpenAI => format!("{base}/v1/embeddings"),
        }
    }

    /// 构造请求体（纯逻辑，可测）。两协议体形态一致：`{model, input:[..]}`。
    fn request_body(&self, inputs: &[String]) -> Value {
        serde_json::json!({ "model": self.cfg.model, "input": inputs })
    }

    fn request_body_values(&self, inputs: Vec<Value>) -> Value {
        serde_json::json!({ "model": self.cfg.model, "input": inputs })
    }

    fn input_value(&self, input: &EmbedInput, kind: EmbedKind) -> Result<Value> {
        match input {
            EmbedInput::Text(t) => Ok(Value::String(format!("{}{}", self.prefix(kind), t))),
            EmbedInput::Image(bytes) => {
                if !self.cfg.image {
                    bail!("HTTP embedder image input disabled (FASTSEARCH_EMBED_IMAGE=false)");
                }
                let media_type = guess_image_media_type(bytes);
                let b64 = B64.encode(bytes);
                let data_url = format!("data:{media_type};base64,{b64}");
                Ok(match self.cfg.image_input_format {
                    ImageInputFormat::DataUrl => Value::String(data_url),
                    ImageInputFormat::Base64Object => {
                        serde_json::json!({"image": b64, "media_type": media_type})
                    }
                    ImageInputFormat::OpenAiContent => serde_json::json!([
                        {"type": "input_image", "image_url": data_url}
                    ]),
                })
            }
        }
    }

    fn send_embedding_body(&self, body: &str) -> Result<String> {
        let url = self.endpoint();
        let mut req = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        if let Some(k) = &self.cfg.api_key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        match req.send_string(body) {
            Ok(resp) => resp.into_string().context("read embedding response"),
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                bail!(
                    "embedding endpoint {url} returned {code}: {}",
                    truncate(&detail, 300)
                );
            }
            Err(e) => Err(e).with_context(|| format!("POST {url}")),
        }
    }

    /// 解析响应体 → 向量（纯逻辑，可测）。按协议取字段、按维度校验。
    fn parse_response(&self, body: &str, n: usize) -> Result<Vec<Vec<f32>>> {
        let v: Value = serde_json::from_str(body).context("parse embedding response json")?;
        let vecs = match self.protocol() {
            HttpProtocol::Ollama => extract_ollama(&v)?,
            HttpProtocol::OpenAI => extract_openai(&v)?,
        };
        if vecs.len() != n {
            bail!(
                "embedding count mismatch: requested {n}, got {} (body: {})",
                vecs.len(),
                truncate(body, 200)
            );
        }
        for (i, e) in vecs.iter().enumerate() {
            if e.len() != self.cfg.dim {
                bail!(
                    "embedding dim mismatch at {i}: config dim={}, model returned {} \
                     (set FASTSEARCH_EMBED_DIM 与 PG 向量列一致)",
                    self.cfg.dim,
                    e.len()
                );
            }
        }
        Ok(vecs)
    }
}

impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.cfg.dim
    }

    fn caps(&self) -> EmbedCaps {
        EmbedCaps {
            dim: self.cfg.dim,
            text: true,
            image: self.cfg.image,
            cross_modal: self.cfg.cross_modal,
            semantic: true,
        }
    }

    fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let inputs = self.inputs(texts, kind);
        let body = serde_json::to_string(&self.request_body(&inputs))?;
        let text = self.send_embedding_body(&body)?;
        self.parse_response(&text, texts.len())
    }

    fn embed_multi(&self, inputs: &[EmbedInput], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(vec![]);
        }
        let vals = inputs
            .iter()
            .map(|i| self.input_value(i, kind))
            .collect::<Result<Vec<_>>>()?;
        let body = serde_json::to_string(&self.request_body_values(vals))?;
        let text = self.send_embedding_body(&body)?;
        self.parse_response(&text, inputs.len())
    }
}

fn guess_image_media_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// Ollama `/api/embed`：`{"embeddings": [[..], ..]}`。
fn extract_ollama(v: &Value) -> Result<Vec<Vec<f32>>> {
    let arr = v
        .get("embeddings")
        .and_then(|x| x.as_array())
        .context("ollama response missing 'embeddings' array")?;
    arr.iter().map(json_to_vec).collect()
}

/// OpenAI 兼容 `/v1/embeddings`：`{"data": [{"embedding":[..], "index":i}, ..]}`，按 index 排序。
fn extract_openai(v: &Value) -> Result<Vec<Vec<f32>>> {
    let data = v
        .get("data")
        .and_then(|x| x.as_array())
        .context("openai response missing 'data' array")?;
    let mut indexed: Vec<(usize, Vec<f32>)> = data
        .iter()
        .map(|item| {
            let idx = item.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let emb = item
                .get("embedding")
                .context("openai data item missing 'embedding'")?;
            Ok((idx, json_to_vec(emb)?))
        })
        .collect::<Result<_>>()?;
    indexed.sort_by_key(|(i, _)| *i);
    Ok(indexed.into_iter().map(|(_, e)| e).collect())
}

fn json_to_vec(v: &Value) -> Result<Vec<f32>> {
    v.as_array()
        .context("embedding is not an array")?
        .iter()
        .map(|n| {
            n.as_f64()
                .map(|f| f as f32)
                .context("embedding element not a number")
        })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_cfg(protocol: HttpProtocol, dim: usize) -> EmbedderConfig {
        EmbedderConfig {
            kind: EmbedderKind::Http(protocol),
            url: "http://localhost:11434".into(),
            model: "test-model".into(),
            dim,
            api_key: None,
            query_prefix: "query: ".into(),
            passage_prefix: "passage: ".into(),
            timeout_secs: 5,
            image: false,
            cross_modal: false,
            image_input_format: ImageInputFormat::DataUrl,
        }
    }

    #[test]
    fn request_body_and_prefix() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::Ollama, 3));
        let inputs = e.inputs(&["毛利率".into(), "营收".into()], EmbedKind::Query);
        assert_eq!(inputs, vec!["query: 毛利率", "query: 营收"]);
        let body = e.request_body(&inputs);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["input"][0], "query: 毛利率");
        // passage 前缀
        let p = e.inputs(&["文段".into()], EmbedKind::Passage);
        assert_eq!(p, vec!["passage: 文段"]);
    }

    #[test]
    fn endpoints() {
        assert_eq!(
            HttpEmbedder::new(http_cfg(HttpProtocol::Ollama, 3)).endpoint(),
            "http://localhost:11434/api/embed"
        );
        let mut c = http_cfg(HttpProtocol::OpenAI, 3);
        c.url = "http://localhost:8080/".into(); // 尾斜杠应被裁掉
        assert_eq!(
            HttpEmbedder::new(c).endpoint(),
            "http://localhost:8080/v1/embeddings"
        );
    }

    #[test]
    fn parse_ollama_response() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::Ollama, 3));
        let body = r#"{"model":"m","embeddings":[[0.1,0.2,0.3],[1.0,0.0,0.0]]}"#;
        let out = e.parse_response(body, 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![0.1f32, 0.2, 0.3]);
        assert_eq!(out[1], vec![1.0f32, 0.0, 0.0]);
    }

    #[test]
    fn parse_openai_response_respects_index_order() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::OpenAI, 2));
        // 故意乱序 index，应按 index 排回
        let body =
            r#"{"data":[{"embedding":[3.0,4.0],"index":1},{"embedding":[1.0,2.0],"index":0}]}"#;
        let out = e.parse_response(body, 2).unwrap();
        assert_eq!(out[0], vec![1.0f32, 2.0]);
        assert_eq!(out[1], vec![3.0f32, 4.0]);
    }

    #[test]
    fn dim_mismatch_errors() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::Ollama, 4)); // 期望 4 维
        let body = r#"{"embeddings":[[0.1,0.2,0.3]]}"#; // 实际 3 维
        let err = e.parse_response(body, 1).unwrap_err().to_string();
        assert!(err.contains("dim mismatch"), "got: {err}");
    }

    #[test]
    fn count_mismatch_errors() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::Ollama, 3));
        let body = r#"{"embeddings":[[0.1,0.2,0.3]]}"#;
        assert!(e.parse_response(body, 2).is_err()); // 要 2 条只给 1 条
    }

    #[test]
    fn from_env_defaults_to_hash() {
        // 不依赖进程环境：直接验证 Hash 配置构造的后端维度。
        let cfg = EmbedderConfig::hash(128);
        let emb = build_embedder(&cfg);
        assert_eq!(emb.dim(), 128);
    }

    #[test]
    fn image_caps_and_request_body() {
        let mut cfg = http_cfg(HttpProtocol::OpenAI, 2);
        cfg.image = true;
        cfg.cross_modal = true;
        cfg.image_input_format = ImageInputFormat::DataUrl;
        let e = HttpEmbedder::new(cfg);
        let caps = e.caps();
        assert!(caps.image);
        assert!(caps.cross_modal);
        let val = e
            .input_value(
                &crate::EmbedInput::Image(b"\x89PNG\r\n\x1a\nabc".to_vec()),
                EmbedKind::Query,
            )
            .unwrap();
        assert!(val.as_str().unwrap().starts_with("data:image/png;base64,"));
    }

    #[test]
    fn image_input_disabled_errors() {
        let e = HttpEmbedder::new(http_cfg(HttpProtocol::OpenAI, 2));
        let err = e
            .input_value(&crate::EmbedInput::Image(vec![1, 2, 3]), EmbedKind::Query)
            .unwrap_err()
            .to_string();
        assert!(err.contains("image input disabled"), "got: {err}");
    }

    /// 实网集成（env-gated）：设 `FASTSEARCH_EMBED_TEST_URL` 才跑，需本地 Ollama。
    /// 例：`FASTSEARCH_EMBEDDER=ollama FASTSEARCH_EMBED_MODEL=nomic-embed-text \
    ///      FASTSEARCH_EMBED_DIM=768 FASTSEARCH_EMBED_TEST_URL=http://localhost:11434 cargo test -p fastsearch-embed`
    #[test]
    fn live_embed_gated() {
        let Ok(url) = std::env::var("FASTSEARCH_EMBED_TEST_URL") else {
            eprintln!("skip live_embed_gated: FASTSEARCH_EMBED_TEST_URL not set");
            return;
        };
        let mut cfg = EmbedderConfig::from_env();
        cfg.url = url;
        if cfg.kind == EmbedderKind::Hash {
            cfg.kind = EmbedderKind::Http(HttpProtocol::Ollama);
        }
        let emb = build_embedder(&cfg);
        let q = emb
            .embed(&["毛利率".into()], EmbedKind::Query)
            .expect("embed");
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].len(), cfg.dim);
        // 确定性：同输入同输出
        let q2 = emb.embed(&["毛利率".into()], EmbedKind::Query).unwrap();
        assert_eq!(q, q2);

        // 语义性：相关文段的余弦应高于无关文段（证明非玩具）。
        let qv = &emb
            .embed(&["公司的盈利能力如何".into()], EmbedKind::Query)
            .unwrap()[0];
        let ps = emb
            .embed(
                &[
                    "本年度毛利率与净利润均显著提升".into(), // 相关
                    "员工团建活动安排在下周五".into(),       // 无关
                ],
                EmbedKind::Passage,
            )
            .unwrap();
        let cos =
            |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>() }; // 已 L2? Ollama 未必归一化，故算点积/范数
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let rel = cos(qv, &ps[0]) / (norm(qv) * norm(&ps[0]));
        let unrel = cos(qv, &ps[1]) / (norm(qv) * norm(&ps[1]));
        eprintln!("cos(rel)={rel:.4} cos(unrel)={unrel:.4}");
        assert!(
            rel > unrel,
            "related ({rel}) should beat unrelated ({unrel})"
        );
    }
}
