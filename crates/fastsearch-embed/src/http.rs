//! 可配置的 HTTP 嵌入后端：把"算向量"委托给外部嵌入服务（**本地 Ollama** 或任意
//! **OpenAI 兼容** `/v1/embeddings` 端点：TEI / vLLM / LM Studio / llama.cpp-server /
//! OpenAI 本身）。同步阻塞（`ureq`，纯 Rust）契合同步 [`Embedder`] trait；服务侧在
//! `spawn_blocking` 里调用即可不阻塞 async 运行时。
//!
//! 选后端用 [`EmbedderConfig`]/[`build_embedder`]/[`EmbedderConfig::from_env`]。请求体构造、
//! 响应解析、维度校验是纯逻辑、有单测；实网调用 env-gated。

use crate::{EmbedCaps, EmbedKind, Embedder, HashEmbedder};
use anyhow::{bail, Context, Result};
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
        }
    }

    /// 从环境变量读取（未设 `FASTSEARCH_EMBEDDER` → Hash 基线，维度取 `FASTSEARCH_EMBED_DIM` 或 384）：
    /// - `FASTSEARCH_EMBEDDER` = `hash` | `ollama` | `openai`
    /// - `FASTSEARCH_EMBED_URL`（默认 ollama `http://localhost:11434`）
    /// - `FASTSEARCH_EMBED_MODEL` / `FASTSEARCH_EMBED_DIM` / `FASTSEARCH_EMBED_API_KEY`
    /// - `FASTSEARCH_EMBED_QUERY_PREFIX` / `FASTSEARCH_EMBED_PASSAGE_PREFIX`
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
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

    /// 当前 HTTP 后端是**文本语义**嵌入；**图像路由 gated**（`image=false`）——需对接多模态端点
    /// （SigLIP-2 / JinaCLIP-v2 / jina-v4 / Voyage / Cohere，base64 input）并确认文图同空间
    /// （`cross_modal`）。该 MM8b 待真多模态模型服务，落地前 `embed_multi(Image)` 走默认实现报错。
    fn caps(&self) -> EmbedCaps {
        EmbedCaps {
            dim: self.cfg.dim,
            text: true,
            image: false,
            cross_modal: false,
            semantic: true,
        }
    }

    fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let inputs = self.inputs(texts, kind);
        let body = serde_json::to_string(&self.request_body(&inputs))?;
        let url = self.endpoint();
        let mut req = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        if let Some(k) = &self.cfg.api_key {
            req = req.set("Authorization", &format!("Bearer {k}"));
        }
        let text = match req.send_string(&body) {
            Ok(resp) => resp.into_string().context("read embedding response")?,
            // ureq 把 4xx/5xx 当 Err(Status)；带上状态码与响应体便于诊断。
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                bail!(
                    "embedding endpoint {url} returned {code}: {}",
                    truncate(&detail, 300)
                );
            }
            Err(e) => return Err(e).with_context(|| format!("POST {url}")),
        };
        self.parse_response(&text, texts.len())
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
