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
    /// 单请求最多嵌入多少条输入（M12）。大 doc 一次全发会触发 413 / 提供方条数上限（OpenAI 2048）/
    /// 超时；超此数则拆多次请求、结果拼接。
    pub max_batch: usize,
    /// 传输/5xx/429 失败的重试次数（M12）；每次指数退避。0=不重试。
    pub retries: u32,
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
            max_batch: 64,
            retries: 2,
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
    /// - `FASTSEARCH_EMBED_TIMEOUT_SECS`（默认 30）/ `FASTSEARCH_EMBED_MAX_BATCH`（默认 64，单请求条数上限）
    ///   / `FASTSEARCH_EMBED_RETRIES`（默认 2，transient 重试次数）（M12）
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
            timeout_secs: var("FASTSEARCH_EMBED_TIMEOUT_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            max_batch: var("FASTSEARCH_EMBED_MAX_BATCH")
                .and_then(|s| s.parse().ok())
                .filter(|&n| n > 0)
                .unwrap_or(64),
            retries: var("FASTSEARCH_EMBED_RETRIES")
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
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

/// 同 [`build_embedder`]，但对 HTTP 后端**先跑一次能力探测**（KB-2.4），
/// 使 `caps()` 之后只宣称实测到的能力。返回 (后端, 探测报告)；Hash 基线不探（无外部服务可探）。
pub fn build_embedder_probed(
    cfg: &EmbedderConfig,
) -> (Box<dyn Embedder + Send + Sync>, ProbeReport) {
    match cfg.kind {
        EmbedderKind::Hash => (Box::new(HashEmbedder::new(cfg.dim)), ProbeReport::skipped()),
        EmbedderKind::Http(_) => {
            let mut e = HttpEmbedder::new(cfg.clone());
            let r = e.probe();
            (Box::new(e), r)
        }
    }
}

/// 能力探测结果（KB-2.4）：**实测**到什么就是什么，与配置声明分开记。
///
/// 存在的理由：`caps()` 此前**直接照抄配置**——`text`/`semantic` 硬编码 `true`，
/// `image`/`cross_modal` 来自 env 开关。没有任何证据表明外部模型真的收图、真的同维、
/// 真的文图同空间。而 `image`/`cross_modal` 是**写入侧和检索侧都要用**的判据
/// （engine 据此决定要不要嵌图、允不允许文→图），照抄配置等于让一个 env 开关
/// 决定检索行为的正确性。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    /// 探测是否真的跑起来了（false = 服务不可达/响应不合法，一切能力均未获证实）。
    pub ran: bool,
    /// 文本嵌入实测可用（数量对、维度对、值有限）。
    pub text_ok: bool,
    /// 图片嵌入实测可用。仅在配置声明 `image` 时才尝试。
    pub image_ok: bool,
    /// 实测维度（文本路）。**只在响应通过下层校验时有值**——`parse_response` 会先按
    /// 配置 `dim` 拒掉不符的响应，那种情况下 probe 根本拿不到向量，失败原因落在 `notes`。
    pub measured_dim: Option<usize>,
    /// 同输入两次是否得到同一向量。**不作为准入条件**——有的服务带随机性/负载均衡到不同副本，
    /// 但不稳定意味着检索结果不可复现，值得让运维看见。
    pub deterministic: Option<bool>,
    /// 人类可读的失败原因（成功时为空）。
    pub notes: Vec<String>,
}

impl ProbeReport {
    /// 未探测（如 Hash 基线、或显式 skip）：一切按配置声明走。
    pub fn skipped() -> Self {
        ProbeReport {
            ran: false,
            text_ok: false,
            image_ok: false,
            measured_dim: None,
            deterministic: None,
            notes: vec!["probe skipped".into()],
        }
    }
}

/// 探测用的最小 PNG（1×1，透明）——固定字节，便于复现。
const PROBE_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// 校验一条向量是否可用。**维度这一条是纵深防御**：HTTP 路的 `parse_response` 已经先拒了
/// 不符的响应，但 `vec_ok` 不该依赖那个前提——换个后端实现就不成立了。
fn vec_ok(v: &[f32], want_dim: usize) -> Option<String> {
    if v.len() != want_dim {
        return Some(format!("维度不符：期望 {want_dim}，实得 {}", v.len()));
    }
    if v.iter().any(|x| !x.is_finite()) {
        return Some("响应含 NaN/Inf".into());
    }
    Some(String::new()).filter(|s| !s.is_empty())
}

/// HTTP 嵌入后端。
pub struct HttpEmbedder {
    cfg: EmbedderConfig,
    agent: ureq::Agent,
    /// 探测结果；`None` = 未探测（`caps()` 退回照抄配置的老行为）。
    probed: Option<ProbeReport>,
}

impl HttpEmbedder {
    pub fn new(cfg: EmbedderConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build();
        HttpEmbedder {
            cfg,
            agent,
            probed: None,
        }
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
        let mut attempt = 0u32;
        loop {
            let mut req = self
                .agent
                .post(&url)
                .set("Content-Type", "application/json");
            if let Some(k) = &self.cfg.api_key {
                req = req.set("Authorization", &format!("Bearer {k}"));
            }
            match req.send_string(body) {
                Ok(resp) => return resp.into_string().context("read embedding response"),
                Err(e) => {
                    // transient（429/5xx/传输错误）才重试；4xx（如 400/413）是确定性错误、立即失败（M12）。
                    let retriable = match &e {
                        ureq::Error::Status(code, _) => *code == 429 || (500..600).contains(code),
                        ureq::Error::Transport(_) => true,
                    };
                    if retriable && attempt < self.cfg.retries {
                        std::thread::sleep(Duration::from_millis(100u64 << attempt)); // 指数退避
                        attempt += 1;
                        continue;
                    }
                    return match e {
                        ureq::Error::Status(code, resp) => {
                            let detail = resp.into_string().unwrap_or_default();
                            bail!(
                                "embedding endpoint {url} returned {code}: {}",
                                truncate(&detail, 300)
                            );
                        }
                        e => Err(e).with_context(|| format!("POST {url}")),
                    };
                }
            }
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
    /// **实测**本后端的能力（KB-2.4）：固定文本 + 固定小图各请求一次，校验数量、维度、有限值，
    /// 并复测一次文本看是否确定。结果写进 `self.probed`，此后 [`caps`](Self::caps) 只报实测到的。
    ///
    /// **为什么必须探**：`caps()` 原本直接照抄配置，而 `image`/`cross_modal` 是引擎在
    /// **写入侧和检索侧**都要用的判据。一个拼错的 env 开关会让引擎以为能文→图，
    /// 于是把图片字节送去嵌入、把文本 query 拿去比图片向量——错得静默且难查。
    ///
    /// **`cross_modal` 探不出来**：证明"文图同空间"需要带标注的跨模态 golden，不是一次请求能做的。
    /// 所以它**仍由配置声明**，但被 `image_ok` 收口——图都嵌不出来，谈何同空间。
    /// 真正的验收在 KB-2.5。
    pub fn probe(&mut self) -> ProbeReport {
        let mut r = ProbeReport {
            ran: true,
            text_ok: false,
            image_ok: false,
            measured_dim: None,
            deterministic: None,
            notes: vec![],
        };
        let probe_text = vec!["fastsearch capability probe".to_string()];
        match self.embed(&probe_text, EmbedKind::Passage) {
            Ok(v) if v.len() == 1 => {
                r.measured_dim = Some(v[0].len());
                match vec_ok(&v[0], self.cfg.dim) {
                    None => {
                        r.text_ok = true;
                        // 复测：不作为准入条件，但不稳定 = 检索结果不可复现，要让运维看见。
                        if let Ok(v2) = self.embed(&probe_text, EmbedKind::Passage) {
                            r.deterministic = Some(v2.first() == v.first());
                        }
                    }
                    Some(why) => r.notes.push(format!("文本嵌入：{why}")),
                }
            }
            Ok(v) => r
                .notes
                .push(format!("文本嵌入：期望 1 条向量，实得 {}", v.len())),
            Err(e) => r.notes.push(format!("文本嵌入失败：{e}")),
        }
        if self.cfg.image {
            match self.embed_multi(&[EmbedInput::Image(PROBE_PNG.to_vec())], EmbedKind::Passage) {
                Ok(v) if v.len() == 1 => match vec_ok(&v[0], self.cfg.dim) {
                    None => r.image_ok = true,
                    Some(why) => r.notes.push(format!("图片嵌入：{why}")),
                },
                Ok(v) => r
                    .notes
                    .push(format!("图片嵌入：期望 1 条向量，实得 {}", v.len())),
                Err(e) => r.notes.push(format!("图片嵌入失败：{e}")),
            }
        }
        self.probed = Some(r.clone());
        r
    }

    /// 最近一次探测结果（未探测则 `None`）。
    pub fn probe_report(&self) -> Option<&ProbeReport> {
        self.probed.as_ref()
    }
}

impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        self.cfg.dim
    }

    /// **宣称的能力 = 实测到的能力**（KB-2.4）。探测跑过就只报实测结果；
    /// 没探过（Hash 基线 / 显式 skip）才退回照抄配置的老行为。
    ///
    /// `cross_modal` 由配置声明**并被 `image` 收口**——图都嵌不出来，谈何文图同空间。
    /// 它本身探不出来（需要跨模态 golden，见 KB-2.5）。
    fn caps(&self) -> EmbedCaps {
        let Some(p) = &self.probed else {
            return EmbedCaps {
                dim: self.cfg.dim,
                text: true,
                image: self.cfg.image,
                cross_modal: self.cfg.cross_modal,
                semantic: true,
            };
        };
        EmbedCaps {
            dim: p.measured_dim.unwrap_or(self.cfg.dim),
            text: p.text_ok,
            image: p.image_ok,
            cross_modal: self.cfg.cross_modal && p.image_ok,
            // 探不出"是否语义"（HashEmbedder 也会返回合法向量）。配了外部模型即按语义算，
            // 但这一条**只是配置声明**，真实语义质量的验收在 KB-2.5。
            semantic: p.text_ok,
        }
    }

    fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // 按 max_batch 拆多次请求（大 doc 一次全发会 413/超条数上限/超时），结果按序拼接（M12）。
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.cfg.max_batch) {
            let inputs = self.inputs(batch, kind);
            let body = serde_json::to_string(&self.request_body(&inputs))?;
            let text = self.send_embedding_body(&body)?;
            out.extend(self.parse_response(&text, batch.len())?);
        }
        Ok(out)
    }

    fn embed_multi(&self, inputs: &[EmbedInput], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(vec![]);
        }
        // 按 max_batch 拆多次请求（含 base64 图片时 body 更易超限），结果按序拼接（M12）。
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(self.cfg.max_batch) {
            let vals = batch
                .iter()
                .map(|i| self.input_value(i, kind))
                .collect::<Result<Vec<_>>>()?;
            let body = serde_json::to_string(&self.request_body_values(vals))?;
            let text = self.send_embedding_body(&body)?;
            out.extend(self.parse_response(&text, batch.len())?);
        }
        Ok(out)
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
            max_batch: 64,
            retries: 0,
            image: false,
            cross_modal: false,
            image_input_format: ImageInputFormat::DataUrl,
        }
    }

    // ---- KB-2.4 能力探测 ----------------------------------------------------

    /// 起一个假嵌入服务：按请求体里有没有图片形态返回不同结果，可注入畸形响应。
    fn spawn_embed_server(
        text_body: &'static str,
        image_body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(16) {
                let Ok(mut st) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match st.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let cl = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() - (p + 4) >= cl {
                            break;
                        }
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                // 图片请求的特征：body 里带 base64 data URL / image 字段。
                let is_image = req.contains("data:image") || req.contains("\"image\"");
                let body = if is_image { image_body } else { text_body };
                let _ = tx.send(req);
                let _ = write!(
                    st,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = st.flush();
            }
        });
        (url, rx)
    }

    fn probe_cfg(url: String, dim: usize, image: bool, cross_modal: bool) -> EmbedderConfig {
        let mut c = http_cfg(HttpProtocol::Ollama, dim);
        c.url = url;
        c.image = image;
        c.cross_modal = cross_modal;
        c
    }

    /// 未探测时保持老行为（照抄配置）——不给既有部署带来行为变化。
    #[test]
    fn caps_without_probe_falls_back_to_config() {
        let e = HttpEmbedder::new(probe_cfg("http://127.0.0.1:1".into(), 3, true, true));
        let c = e.caps();
        assert!(c.text && c.image && c.cross_modal, "未探测 → 照抄配置");
    }

    /// **维度不符必须让 text_ok=false**：配置说 3 维、服务返 2 维，此前 `caps()` 照样宣称可用。
    #[test]
    fn probe_catches_dimension_mismatch() {
        let (url, _rx) = spawn_embed_server(r#"{"embeddings":[[1.0,2.0]]}"#, "{}");
        let mut e = HttpEmbedder::new(probe_cfg(url, 3, false, false));
        let r = e.probe();
        assert!(r.ran && !r.text_ok, "维度不符不得算通过");
        // `measured_dim` 在这里是 None，**这是对的**：维度校验在 `parse_response` 那一层
        // 就已经把请求拒了（见 `dim_mismatch_errors`），probe 拿不到向量本身。
        // 实测维度只在响应通过下层校验时才有值；不符的情形由 notes 里的下层错误交代。
        assert_eq!(r.measured_dim, None);
        assert!(
            r.notes.iter().any(|n| n.contains("文本嵌入失败")),
            "失败原因要带上下层的错误：{:?}",
            r.notes
        );
        assert!(!e.caps().text, "caps 必须跟着实测走");
    }

    /// **声明了 image 但服务嵌不出图** ⇒ `image` 与 `cross_modal` 双双落地。
    /// cross_modal 被 image 收口：图都嵌不出来，谈何文图同空间。
    #[test]
    fn probe_downgrades_image_and_cross_modal_when_image_fails() {
        // 图片请求返回畸形响应（没有 embeddings 字段）。
        let (url, _rx) = spawn_embed_server(r#"{"embeddings":[[1.0,2.0,3.0]]}"#, r#"{"oops":1}"#);
        let mut e = HttpEmbedder::new(probe_cfg(url, 3, true, true));
        let r = e.probe();
        assert!(r.text_ok, "文本路应通过");
        assert!(!r.image_ok, "图片路应失败");
        let c = e.caps();
        assert!(c.text, "文本能力保留");
        assert!(!c.image, "宣称的 image 必须被实测推翻");
        assert!(
            !c.cross_modal,
            "cross_modal 被 image 收口——嵌不出图就不可能同空间"
        );
    }

    /// 服务不可达 ⇒ 一切能力均未获证实，`caps` 全线降级（而不是继续宣称）。
    #[test]
    fn probe_unreachable_service_downgrades_everything() {
        let mut e = HttpEmbedder::new(probe_cfg("http://127.0.0.1:1".into(), 3, true, true));
        let r = e.probe();
        assert!(r.ran && !r.text_ok && !r.image_ok);
        assert!(r.notes.iter().any(|n| n.contains("失败")), "{:?}", r.notes);
        let c = e.caps();
        assert!(!c.text && !c.image && !c.cross_modal && !c.semantic);
    }

    /// 全绿路径：文本 + 图片都通过，且复测一致 ⇒ 记 deterministic=true。
    #[test]
    fn probe_all_green_records_determinism() {
        let (url, _rx) = spawn_embed_server(
            r#"{"embeddings":[[1.0,2.0,3.0]]}"#,
            r#"{"embeddings":[[4.0,5.0,6.0]]}"#,
        );
        let mut e = HttpEmbedder::new(probe_cfg(url, 3, true, true));
        let r = e.probe();
        assert!(r.text_ok && r.image_ok);
        assert_eq!(r.deterministic, Some(true));
        let c = e.caps();
        assert!(c.text && c.image && c.cross_modal && c.semantic);
        assert_eq!(c.dim, 3);
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

    /// 极简 mock 嵌入服务：前 `fail_first` 个请求回 503（测重试），其余按请求体的 input 条数回
    /// 等量 Ollama 向量。返回 (base_url, 请求计数)。
    fn mock_embed_server(
        dim: usize,
        fail_first: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // 读到 header 结束 + Content-Length 个 body 字节（localhost 小请求，稳妥读全）。
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (mut headers_end, mut want) = (None, 0usize);
                loop {
                    let n = match s.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if headers_end.is_none() {
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            headers_end = Some(p + 4);
                            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                            want = head
                                .split("content-length:")
                                .nth(1)
                                .and_then(|s| s.split("\r\n").next())
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                    if let Some(h) = headers_end {
                        if buf.len() >= h + want {
                            break;
                        }
                    }
                }
                let idx = c2.fetch_add(1, Ordering::SeqCst);
                if idx < fail_first {
                    let _ = s.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 3\r\n\r\nerr",
                    );
                    continue;
                }
                let body = headers_end
                    .map(|h| String::from_utf8_lossy(&buf[h..]).to_string())
                    .unwrap_or_default();
                let v: Value = serde_json::from_str(&body).unwrap_or(serde_json::json!({}));
                let n_in = v["input"].as_array().map(|a| a.len()).unwrap_or(0);
                let embs: Vec<Vec<f32>> = (0..n_in).map(|_| vec![0.1f32; dim]).collect();
                let rb = serde_json::json!({ "embeddings": embs }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    rb.len(),
                    rb
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}"), count)
    }

    #[test]
    fn embed_splits_into_batches() {
        // M12：max_batch=2 时 5 条输入应拆成 3 次请求（2+2+1），结果按序拼接为 5 条。
        use std::sync::atomic::Ordering;
        let (url, count) = mock_embed_server(4, 0);
        let mut cfg = http_cfg(HttpProtocol::Ollama, 4);
        cfg.url = url;
        cfg.max_batch = 2;
        let e = HttpEmbedder::new(cfg);
        let texts: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let out = e.embed(&texts, EmbedKind::Passage).unwrap();
        assert_eq!(out.len(), 5, "5 条输入应返回 5 个向量");
        assert!(out.iter().all(|v| v.len() == 4));
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "5 条 / max_batch=2 → 3 次请求"
        );
    }

    #[test]
    fn embed_retries_transient_5xx() {
        // M12：前一个请求 503（transient），retries=2 应重试并最终成功。
        let (url, _count) = mock_embed_server(4, 1);
        let mut cfg = http_cfg(HttpProtocol::Ollama, 4);
        cfg.url = url;
        cfg.retries = 2;
        let e = HttpEmbedder::new(cfg);
        let out = e.embed(&["hi".into()], EmbedKind::Passage).unwrap();
        assert_eq!(out.len(), 1, "重试后应成功拿到 1 个向量");
    }

    #[test]
    fn embed_no_retry_gives_up_on_5xx() {
        // 对照：retries=0 时 503 立即失败（不无限重试）。
        let (url, _count) = mock_embed_server(4, 1);
        let mut cfg = http_cfg(HttpProtocol::Ollama, 4);
        cfg.url = url;
        cfg.retries = 0;
        let e = HttpEmbedder::new(cfg);
        assert!(e.embed(&["hi".into()], EmbedKind::Passage).is_err());
    }
}
