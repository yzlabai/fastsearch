//! # fastsearch-cli (lib) — server 的**纯 REST 客户端**（四张脸之一）
//!
//! 检索/嵌入/落盘全归 server；本 crate 只做：命令→REST 端点映射 + **客户端侧分块/解析**
//! （`chunk_text` 纯函数 / docparse `ingest`）+ I/O。**不嵌引擎**（无 text/vector/engine 依赖）。
//! 与 `clients/{python,ts}` 同模型——业界服务端检索产品（Typesense/Qdrant/Algolia/Meilisearch）
//! 的 CLI 皆为瘦 HTTP 客户端。详见 [spec](../../docs/specs/17-cli.md) 与
//! [设计](../../docs/plans/2026-06-28-CLI改为REST客户端设计.md)。

#[cfg(feature = "parse")]
pub mod ingest;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use fastsearch_core::{
    BBox, Chunk, ChunkKind, FieldValue, Filter, GlobalId, SearchMode, SearchRequest,
};
use fastsearch_eval::{evaluate, GoldenSet, Metrics, RankedResults};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

// ============================ HTTP 客户端（瘦封装 ureq） ============================

/// server REST 客户端。`base` 末尾无 `/`；`key` 作 `Authorization: Bearer`。
pub struct Client {
    base: String,
    key: String,
}

impl Client {
    /// 解析连接配置：显式 > env（`FASTSEARCH_SERVER`/`FASTSEARCH_KEY`）> 默认 localhost:8642。
    pub fn new(server: Option<String>, key: Option<String>) -> Self {
        let base = server
            .or_else(|| std::env::var("FASTSEARCH_SERVER").ok())
            .unwrap_or_else(|| "http://localhost:8642".to_string());
        let key = key
            .or_else(|| std::env::var("FASTSEARCH_KEY").ok())
            .unwrap_or_default();
        Client {
            base: base.trim_end_matches('/').to_string(),
            key,
        }
    }

    /// 底层 POST：返回**类型化错误**，让 `post_retry` 精确区分"状态码拒绝"（不重试）vs
    /// "传输失败"（可重试）——不靠脆弱的错误字符串匹配。
    fn post_raw<B: Serialize>(&self, url: &str, body: &B) -> std::result::Result<Value, PostError> {
        match ureq::post(url)
            .set("authorization", &format!("Bearer {}", self.key))
            .send_json(body)
        {
            Ok(r) => r
                .into_json()
                .map_err(|e| PostError::Transport(e.to_string())),
            Err(ureq::Error::Status(code, r)) => {
                Err(PostError::Status(code, r.into_string().unwrap_or_default()))
            }
            Err(e) => Err(PostError::Transport(e.to_string())),
        }
    }

    fn post<B: Serialize>(&self, path: &str, body: &B) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        self.post_raw(&url, body).map_err(|e| e.into_anyhow(&url))
    }

    fn post_multipart(&self, path: &str, boundary: &str, body: Vec<u8>) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        match ureq::post(&url)
            .set("authorization", &format!("Bearer {}", self.key))
            .set(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send_bytes(&body)
        {
            Ok(r) => r
                .into_json()
                .map_err(|e| anyhow!("server returned non-json response: {e}")),
            Err(ureq::Error::Status(code, r)) => Err(anyhow!(
                "server 返回 {code}: {}",
                r.into_string().unwrap_or_default()
            )),
            Err(e) => Err(anyhow!(
                "请求 {url} 失败：{e}（server 在运行吗？检查 --server / FASTSEARCH_SERVER）"
            )),
        }
    }

    /// 带瞬时失败重试的 POST（批量写入用）：仅 `Transport`（连接/读写失败）重试；
    /// `Status`（4xx/5xx 确定性拒绝）立即抛、不重试。
    fn post_retry<B: Serialize>(&self, path: &str, body: &B, retries: usize) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        for attempt in 0..=retries {
            match self.post_raw(&url, body) {
                Ok(v) => return Ok(v),
                Err(e @ PostError::Status(..)) => return Err(e.into_anyhow(&url)),
                Err(e) => {
                    if attempt == retries {
                        return Err(e.into_anyhow(&url));
                    }
                    eprintln!("  传输失败，重试 {}/{retries}…", attempt + 1);
                }
            }
        }
        unreachable!("loop returns on last attempt")
    }
}

/// POST 失败的类型化原因：`Status`=server 返回非 2xx（确定性拒绝）；`Transport`=连接/读写失败（可重试）。
enum PostError {
    Status(u16, String),
    Transport(String),
}

impl PostError {
    fn into_anyhow(self, url: &str) -> anyhow::Error {
        match self {
            PostError::Status(code, msg) => anyhow!("server 返回 {code}: {msg}"),
            PostError::Transport(e) => anyhow!(
                "请求 {url} 失败：{e}（server 在运行吗？检查 --server / FASTSEARCH_SERVER）"
            ),
        }
    }
}

// 请求体（镜像 server `IndexBody`/`SimilarBody`；search 直接发 `core::SearchRequest`）。
#[derive(Serialize)]
struct IndexBody<'a> {
    collection: &'a str,
    doc_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_media: Option<StoreMedia>,
    chunks: &'a [Chunk],
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreMedia {
    Inline,
    Auto,
    Object,
    Reference,
}

#[derive(Serialize)]
struct SimilarBody<'a> {
    citation_id: &'a str,
    top_k: usize,
}

/// POST 一个 doc 的 chunks 到 `/v1/index`（带重试）。返回 server 报告的 indexed 数。
fn post_index(
    client: &Client,
    collection: &str,
    doc_id: &str,
    store_media: Option<StoreMedia>,
    chunks: &[Chunk],
) -> Result<usize> {
    let body = IndexBody {
        collection,
        doc_id,
        store_media,
        chunks,
    };
    let v = client.post_retry("/v1/index", &body, 3)?;
    Ok(v["indexed"].as_u64().unwrap_or(0) as usize)
}

/// 取响应里的 `hits` 数组（search/similar 共用）。
fn hits_of(v: Value) -> Vec<Value> {
    match v.get("hits") {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    }
}

// ============================ docparse chunks 解析（客户端侧，纯函数） ============================

/// docparse `-f chunks` 的单个 chunk（字段 `id`，无 doc_id/acl）。
#[derive(Debug, serde::Deserialize)]
struct DocparseChunk {
    id: u64,
    kind: ChunkKind,
    text: String,
    page: u32,
    bbox: BBox,
    #[serde(default)]
    heading_path: Vec<String>,
    #[serde(default)]
    section_id: u64,
    char_len: u32,
    #[serde(default)]
    image: Option<fastsearch_core::ImageMeta>,
}

fn to_core(dc: DocparseChunk, doc_id: &str) -> Chunk {
    let media_bytes = dc.image.as_ref().and_then(decode_image_bytes);
    Chunk {
        doc_id: doc_id.to_string(),
        chunk_id: dc.id,
        kind: dc.kind,
        text: dc.text,
        page: dc.page,
        bbox: dc.bbox,
        heading_path: dc.heading_path,
        section_id: dc.section_id,
        char_len: dc.char_len,
        media: dc.image.as_ref().map(|im| im.to_media(dc.page, dc.bbox)),
        media_bytes,
        image_vector_status: None,
        tenant: None,
        acl: vec!["public".to_string()],
    }
}

fn decode_image_bytes(im: &fastsearch_core::ImageMeta) -> Option<Vec<u8>> {
    let raw = im.data_base64.as_ref()?.trim();
    let b64 = raw.rsplit_once(',').map(|(_, v)| v).unwrap_or(raw);
    B64.decode(b64).ok()
}

/// 解析 docparse chunks（JSON 数组 或 NDJSON），注入 doc_id → core::Chunk。
pub fn parse_chunks(bytes: &[u8], doc_id: &str) -> Result<Vec<Chunk>> {
    let s = std::str::from_utf8(bytes).context("input is not valid UTF-8")?;
    let trimmed = s.trim_start();
    if trimmed.is_empty() {
        return Ok(vec![]);
    }
    if trimmed.starts_with('[') {
        let arr: Vec<DocparseChunk> =
            serde_json::from_str(trimmed).context("parsing JSON array of chunks")?;
        Ok(arr.into_iter().map(|c| to_core(c, doc_id)).collect())
    } else {
        let mut out = Vec::new();
        for (i, line) in s.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let c: DocparseChunk = serde_json::from_str(line)
                .map_err(|e| anyhow!("parse error on line {}: {e}", i + 1))?;
            out.push(to_core(c, doc_id));
        }
        Ok(out)
    }
}

// ============================ 文本/markdown 分块（客户端侧，纯函数） ============================

/// 解析 markdown 标题行 → `(层级, 标题)`；非标题返回 None。
fn parse_md_heading(line: &str) -> Option<(usize, String)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let title = t[level..].trim();
    if title.is_empty() || !t[level..].starts_with(char::is_whitespace) {
        return None;
    }
    Some((level, title.to_string()))
}

fn heading_titles(path: &[(usize, String)]) -> Vec<String> {
    path.iter().map(|(_, t)| t.clone()).collect()
}

fn mk_text_chunk(
    doc_id: &str,
    id: &mut u64,
    kind: ChunkKind,
    text: String,
    hp: Vec<String>,
) -> Chunk {
    let c = Chunk {
        doc_id: doc_id.to_string(),
        chunk_id: *id,
        kind,
        char_len: text.chars().count() as u32,
        text,
        page: 1,
        bbox: BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 0.0,
            y1: 0.0,
        },
        heading_path: hp,
        section_id: 0,
        media: None,
        media_bytes: None,
        image_vector_status: None,
        tenant: None,
        acl: vec!["public".to_string()],
    };
    *id += 1;
    c
}

fn flush_para(
    buf: &mut Vec<&str>,
    chunks: &mut Vec<Chunk>,
    next: &mut u64,
    doc_id: &str,
    path: &[(usize, String)],
) {
    if buf.is_empty() {
        return;
    }
    let text = buf.join(" ").trim().to_string();
    buf.clear();
    if !text.is_empty() {
        chunks.push(mk_text_chunk(
            doc_id,
            next,
            ChunkKind::Paragraph,
            text,
            heading_titles(path),
        ));
    }
}

/// 把纯文本 / markdown 内容切成 chunk：**空行分段**；markdown 标题（`# …`）更新 `heading_path`
/// 并自成一个 `Heading` chunk，正文段为 `Paragraph`。供"喂一个文件夹"客户端分块后上传。
pub fn chunk_text(content: &str, doc_id: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut next = 0u64;
    for line in content.lines() {
        if let Some((level, title)) = parse_md_heading(line) {
            flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
            while path.last().map(|(l, _)| *l >= level).unwrap_or(false) {
                path.pop();
            }
            path.push((level, title.clone()));
            chunks.push(mk_text_chunk(
                doc_id,
                &mut next,
                ChunkKind::Heading,
                title,
                heading_titles(&path),
            ));
        } else if line.trim().is_empty() {
            flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
        } else {
            buf.push(line);
        }
    }
    flush_para(&mut buf, &mut chunks, &mut next, doc_id, &path);
    chunks
}

// ============================ 命令 ============================

/// index 选项（chunks.json → POST /v1/index）。
pub struct IndexOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    pub doc_id: String,
    pub store_media: Option<StoreMedia>,
}

/// 原始图片上传选项（multipart → `/v1/images`）。
pub struct ImageUploadOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    pub doc_id: String,
    pub text: Option<String>,
    pub page: u32,
    pub store_media: Option<StoreMedia>,
}

/// 灌入一个 doc 的 chunks（chunks.json/NDJSON → server）。返回 indexed 数。
pub fn cmd_index(opts: &IndexOpts, input: &[u8]) -> Result<usize> {
    let chunks = parse_chunks(input, &opts.doc_id)?;
    let client = Client::new(opts.server.clone(), opts.key.clone());
    post_index(
        &client,
        &opts.collection,
        &opts.doc_id,
        opts.store_media,
        &chunks,
    )
}

fn media_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
}

fn multipart_text(out: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

fn multipart_file(
    out: &mut Vec<u8>,
    boundary: &str,
    field: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) {
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// 上传一个原始图片文件到 `/v1/images`，由 server 负责对象存储、嵌入、索引和真源写入。
pub fn cmd_upload_image(opts: &ImageUploadOpts, path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let client = Client::new(opts.server.clone(), opts.key.clone());
    let boundary = format!(
        "fastsearch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut body = Vec::new();
    multipart_text(&mut body, &boundary, "collection", &opts.collection);
    multipart_text(&mut body, &boundary, "doc_id", &opts.doc_id);
    multipart_text(&mut body, &boundary, "page", &opts.page.to_string());
    if let Some(text) = &opts.text {
        multipart_text(&mut body, &boundary, "text", text);
    }
    if let Some(store_media) = opts.store_media {
        let raw = match store_media {
            StoreMedia::Inline => "inline",
            StoreMedia::Auto => "auto",
            StoreMedia::Object => "object",
            StoreMedia::Reference => "reference",
        };
        multipart_text(&mut body, &boundary, "store_media", raw);
    }
    let filename = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("image.bin");
    multipart_file(
        &mut body,
        &boundary,
        "image",
        filename,
        media_type_for_path(path),
        &bytes,
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    client.post_multipart("/v1/images", &boundary, body)
}

/// index-dir 选项（喂整个文件夹 → 客户端分块 → POST /v1/index）。
pub struct IndexDirOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    /// 并发上传文件数（≥1）。大文件夹用并发抵消单文件 POST 往返延迟。
    pub concurrency: usize,
}

fn is_text_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown") | Some("txt") | Some("text")
    )
}

fn collect_text_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let p = entry?.path();
        if p.is_dir() {
            collect_text_files(&p, out)?;
        } else if is_text_file(&p) {
            out.push(p);
        }
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 读文件 → 分块 → POST `/v1/index`（doc_id=rel）。返回 chunk 数。
fn read_chunk_post(client: &Client, collection: &str, rel: &str, path: &Path) -> Result<usize> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let chunks = chunk_text(&content, rel);
    post_index(client, collection, rel, None, &chunks)
}

/// **喂一个文件夹**：递归遍历 `root` 下的 .md/.txt（确定性排序），每文件 `chunk_text` 切块、
/// 按**文件**POST 到 `/v1/index`（`doc_id`=相对路径）。**有界并发**（`opts.concurrency`）抵消单
/// 文件 POST 往返延迟 + 进度输出 + 逐文件 continue-on-error。返回 `(成功, 失败, chunk 总数)`。
/// 计数确定（原子聚合）；并发下进度行可能交错（无碍正确性，每文件独立 doc）。
pub fn cmd_index_dir(opts: &IndexDirOpts, root: &Path) -> Result<(usize, usize, usize)> {
    let client = Client::new(opts.server.clone(), opts.key.clone());
    let mut files = Vec::new();
    collect_text_files(root, &mut files)?;
    files.sort(); // 确定性 doc_id 分配
    let n = files.len();
    let (ok, failed, total, done) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let cursor = AtomicUsize::new(0);
    let workers = opts.concurrency.max(1).min(n.max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let rel = rel_path(root, &files[i]);
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                match read_chunk_post(&client, &opts.collection, &rel, &files[i]) {
                    Ok(cn) => {
                        eprintln!("  [{d}/{n}] {rel} → {cn} chunk(s)");
                        ok.fetch_add(1, Ordering::Relaxed);
                        total.fetch_add(cn, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("  [{d}/{n}] {rel} 失败：{e}（跳过）");
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    Ok((ok.into_inner(), failed.into_inner(), total.into_inner()))
}

/// search 选项。
pub struct SearchOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    pub query: String,
    pub mode: SearchMode,
    pub top_k: usize,
    pub kind: Option<String>,
    pub modality: Option<String>,
    pub image: Option<PathBuf>,
    pub page_min: Option<u32>,
    pub page_max: Option<u32>,
}

/// 由 collection（必加，限定作用域）+ 可选 kind/page 范围构造过滤。
pub fn build_filter(
    collection: &str,
    kind: Option<&str>,
    modality: Option<&str>,
    page_min: Option<u32>,
    page_max: Option<u32>,
) -> Filter {
    let mut clauses = vec![Filter::Eq(
        "collection".into(),
        FieldValue::Str(collection.to_string()),
    )];
    if let Some(k) = kind {
        clauses.push(Filter::Eq("kind".into(), FieldValue::Str(k.to_string())));
    }
    if let Some(m) = modality {
        clauses.push(Filter::Eq(
            "modality".into(),
            FieldValue::Str(m.to_string()),
        ));
    }
    if let Some(lo) = page_min {
        clauses.push(Filter::Gte("page".into(), FieldValue::Int(lo as i64)));
    }
    if let Some(hi) = page_max {
        clauses.push(Filter::Lte("page".into(), FieldValue::Int(hi as i64)));
    }
    if clauses.len() == 1 {
        clauses.pop().unwrap()
    } else {
        Filter::And(clauses)
    }
}

/// 检索（经 server `/v1/search`）。返回 server 的 hit 对象数组（原样透传，便于 `--json`/agent）。
pub fn cmd_search(opts: &SearchOpts) -> Result<Vec<Value>> {
    let client = Client::new(opts.server.clone(), opts.key.clone());
    let req = SearchRequest {
        query: opts.query.clone(),
        mode: opts.mode,
        top_k: opts.top_k,
        candidates: opts.top_k.max(150),
        filter: Some(build_filter(
            &opts.collection,
            opts.kind.as_deref(),
            opts.modality.as_deref(),
            opts.page_min,
            opts.page_max,
        )),
        ..Default::default()
    };
    let mut body = serde_json::to_value(&req)?;
    if let Some(path) = &opts.image {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        body["query_image_base64"] = Value::String(B64.encode(bytes));
    }
    Ok(hits_of(client.post("/v1/search", &body)?))
}

/// similar 选项。
pub struct SimilarOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub citation_id: String,
    pub top_k: usize,
}

/// more_like_this（经 server `/v1/similar`）。
pub fn cmd_similar(opts: &SimilarOpts) -> Result<Vec<Value>> {
    let client = Client::new(opts.server.clone(), opts.key.clone());
    let body = SimilarBody {
        citation_id: &opts.citation_id,
        top_k: opts.top_k,
    };
    Ok(hits_of(client.post("/v1/similar", &body)?))
}

/// eval 选项。
pub struct EvalOpts {
    pub server: Option<String>,
    pub key: Option<String>,
    pub golden: PathBuf,
    pub baseline: Option<PathBuf>,
    pub tol: f64,
    pub k: usize,
    pub mode: SearchMode,
}

/// 相关性评测（客户端化）：golden 语料按 doc 分组灌入其 `collection` → 逐查询经 server 检索 →
/// `fastsearch-eval` 算 nDCG/recall/MRR/precision；给 baseline 则回归门禁。
/// **注**：会把 golden 语料灌进目标 server 的 `set.collection`——请指向专用/临时集合或测试 server。
pub fn cmd_eval(opts: &EvalOpts) -> Result<(Metrics, Option<std::result::Result<(), String>>)> {
    let raw = std::fs::read_to_string(&opts.golden)
        .with_context(|| format!("reading golden {}", opts.golden.display()))?;
    let set = GoldenSet::from_json(&raw).context("parsing golden set")?;
    let client = Client::new(opts.server.clone(), opts.key.clone());

    // 语料按 doc_id 分组 → 逐 doc POST 入库（doc 级替换语义由 server 保证）。
    let mut by_doc: std::collections::BTreeMap<String, Vec<Chunk>> = Default::default();
    for c in &set.corpus {
        by_doc.entry(c.doc_id.clone()).or_default().push(c.clone());
    }
    for (doc_id, chunks) in &by_doc {
        post_index(&client, &set.collection, doc_id, None, chunks)?;
    }

    // 逐查询检索 → 收集排名 GlobalId（限定到 set.collection）。
    let mut results = RankedResults::new();
    for q in &set.queries {
        let req = SearchRequest {
            query: q.query.clone(),
            mode: opts.mode,
            top_k: opts.k,
            candidates: opts.k.max(150),
            filter: Some(build_filter(&set.collection, None, None, None, None)),
            ..Default::default()
        };
        let hits = hits_of(client.post("/v1/search", &req)?);
        let ranked: Vec<GlobalId> = hits
            .iter()
            .filter_map(|h| h.get("citation_id").and_then(|v| v.as_str()))
            .filter_map(|cid| GlobalId::parse(cid).ok())
            .collect();
        results.set(q.query.clone(), ranked);
    }

    let judg = set.judgments().context("building judgments")?;
    let metrics = evaluate(&results, &judg, opts.k);

    let gate = match &opts.baseline {
        Some(p) => {
            let b =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            let base: Metrics = serde_json::from_str(&b).context("parsing baseline metrics")?;
            Some(fastsearch_eval::assert_no_regression(
                &base, &metrics, opts.tol,
            ))
        }
        None => None,
    };
    Ok((metrics, gate))
}

#[cfg(test)]
mod tests;
