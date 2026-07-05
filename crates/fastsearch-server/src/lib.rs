//! # fastsearch-server
//!
//! REST 服务：API-Key 认证 + **逐文档 ACL 不可绕过**（服务端注入）+ 基础可观测。
//! 详见 [spec](../../docs/specs/19-server.md)。
//!
//! 安全核心（需求 F44）：ACL 只来自认证身份，客户端无法在请求体里传 ACL 或越权。

use axum::{
    body::{to_bytes, Body},
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL},
    Engine as _,
};
use fastsearch_core::{
    AclFilter, AssetPointer, BBox, Chunk, ChunkKind, FieldValue, Filter, GlobalId, MediaRef,
    PublicMediaRef, SearchMode, SearchRequest,
};
use fastsearch_embed::{EmbedInput, EmbedKind, Embedder};
use fastsearch_engine::Engine;
use fastsearch_engine::{AssetFetch, ObjectSigner};
use fastsearch_sync::replication::ReplicationConfig;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectTokenTarget {
    cid: String,
    uri: String,
}

/// Object token 密文上限（字节）：典型 token ~200B（cid+uri JSON + 17B 头）。上限留 16x 余量
/// 防止恶意大 token 触发 B64URL 解码 / JSON 解析的内存尖峰（与速率限制互补）。
const OBJECT_TOKEN_MAX_BYTES: usize = 4 * 1024;
const OBJECT_TOKEN_MAX_ENCODED_BYTES: usize = b64url_unpadded_len(OBJECT_TOKEN_MAX_BYTES);

const fn b64url_unpadded_len(decoded_len: usize) -> usize {
    let full = (decoded_len / 3) * 4;
    match decoded_len % 3 {
        0 => full,
        1 => full + 2,
        _ => full + 3,
    }
}

fn object_token_encoded_len_ok(token: &str) -> bool {
    token.len() <= OBJECT_TOKEN_MAX_ENCODED_BYTES
}

/// 资产 URL 签名器（MM6-signer）：用 HMAC-SHA256 对 `cid|exp|ct` 签短时 token，让前端 `<img src>`
/// **免 Bearer 头**取 inline 字节。签名即"已授权"凭证（presigned 语义）：拿到合法 URL 的前提是
/// resolve 时过了 ACL（S3 `/v1/assets/resolve` 签发）；字节端点只验签、不再查 ACL（守不变量 #3）。
/// cid+exp+ct 全入 MAC → 不可篡改/挪用/过期复用。无状态（密钥 env 共享）→ 多副本一致。
pub struct AssetSigner {
    key: Vec<u8>,
    ttl_secs: u64,
}

impl AssetSigner {
    pub fn new(key: Vec<u8>, ttl_secs: u64) -> Self {
        AssetSigner { key, ttl_secs }
    }

    fn mac_bytes(&self, msg: &[u8]) -> Vec<u8> {
        use hmac::digest::KeyInit; // new_from_slice
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC key 任意长度");
        mac.update(msg);
        mac.finalize().into_bytes().to_vec()
    }

    fn mac_hex(&self, cid: &str, exp: u64, ct: &str) -> String {
        // 长度前缀 framing 消除分隔符歧义：cid 内嵌客户端可控的 doc_id、ct 来自客户端 media_type，
        // 二者可含任意字符（含 `|`）。旧 `cid|exp|ct` 拼接下 (cid1,ct1) 与某个 (cid2,ct2) 可拼出逐字节
        // 相等的消息 → 签名跨对复用。每字段前置其字节长度（8B BE），字段边界唯一可解析（M20）。
        let mut msg = Vec::with_capacity(cid.len() + ct.len() + 24);
        for field in [cid.as_bytes(), ct.as_bytes()] {
            msg.extend_from_slice(&(field.len() as u64).to_be_bytes());
            msg.extend_from_slice(field);
        }
        msg.extend_from_slice(&exp.to_be_bytes());
        self.mac_bytes(&msg)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 签发：返回 `(exp, sig)`。`now`=当前 unix 秒（外部传入，便于纯测）。
    pub fn sign(&self, cid: &str, ct: &str, now: u64) -> (u64, String) {
        let exp = now + self.ttl_secs;
        (exp, self.mac_hex(cid, exp, ct))
    }

    /// 验签：sig 与 `HMAC(cid|exp|ct)` **常量时间**相等 且 未过期。
    pub fn verify(&self, cid: &str, exp: u64, ct: &str, sig: &str, now: u64) -> bool {
        if exp <= now {
            return false;
        }
        ct_eq(self.mac_hex(cid, exp, ct).as_bytes(), sig.as_bytes())
    }

    fn object_token_keystream(&self, nonce: &[u8; 16], len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut block = 0u64;
        while out.len() < len {
            let mut msg = b"object-token-stream|".to_vec();
            msg.extend_from_slice(nonce);
            msg.extend_from_slice(&block.to_be_bytes());
            out.extend_from_slice(&self.mac_bytes(&msg));
            block = block.saturating_add(1);
        }
        out.truncate(len);
        out
    }

    fn seal_object_target(&self, target: &ObjectTokenTarget) -> String {
        let plain = serde_json::to_vec(target).expect("object token target serializes");
        let nonce = object_token_nonce();
        let stream = self.object_token_keystream(&nonce, plain.len());
        let cipher: Vec<u8> = plain.iter().zip(stream).map(|(p, k)| p ^ k).collect();
        let mut raw = Vec::with_capacity(1 + nonce.len() + cipher.len());
        raw.push(1);
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&cipher);
        B64URL.encode(raw)
    }

    fn open_object_target(&self, token: &str) -> Option<ObjectTokenTarget> {
        if !object_token_encoded_len_ok(token) {
            return None;
        }
        let raw = B64URL.decode(token.as_bytes()).ok()?;
        // 解码后仍复核，避免非标准/未来编码器绕过 encoded 长度上限。
        if raw.len() < 17 || raw.len() > OBJECT_TOKEN_MAX_BYTES || raw[0] != 1 {
            return None;
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&raw[1..17]);
        let cipher = &raw[17..];
        let stream = self.object_token_keystream(&nonce, cipher.len());
        let plain: Vec<u8> = cipher.iter().zip(stream).map(|(c, k)| c ^ k).collect();
        serde_json::from_slice(&plain).ok()
    }
}

/// 常量时间比较（等长才可能相等；异或折叠，无早退）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn object_token_nonce() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // OS CSPRNG：128 bit 熵 → 多副本/重启后 nonce 碰撞概率 ~2⁻¹²⁸/token，无需依赖时间/PID。
    // getrandom 已作为 workspace dep 引入；OS 取随机失败视为异常 → 回落到时间+PID（降级但不 panic）。
    if getrandom::getrandom(&mut nonce).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        nonce[..8].copy_from_slice(&nanos.to_be_bytes());
        nonce[8..].copy_from_slice(&((std::process::id() as u64).to_be_bytes()));
    }
    nonce
}

/// 百分号编码（RFC 3986 unreserved 不编，其余 `%XX`）——cid/ct 放进 URL 路径/查询安全（doc_id 可含空格等）。
fn pct(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// 签发 inline 字节的短时 URL（前端 `<img src>` 用）。返回 `(url, expires_s)`。签名对**原始** cid/ct
/// （字节端点 `Path`/`Query` 解码后验签一致）；URL 里 cid/ct 百分号编码。
fn mint_inline_url(
    signer: &AssetSigner,
    cid: &str,
    media_type: Option<&str>,
    now: u64,
) -> (String, u64) {
    let ct = media_type.unwrap_or("application/octet-stream");
    let (exp, sig) = signer.sign(cid, ct, now);
    let url = format!(
        "/v1/asset/{}/bytes?exp={exp}&ct={}&sig={sig}",
        pct(cid),
        pct(ct)
    );
    (url, exp.saturating_sub(now))
}

fn object_mac_id(token: &str) -> String {
    format!("object:{token}")
}

fn mint_object_url(
    signer: &AssetSigner,
    cid: &str,
    uri: &str,
    media_type: Option<&str>,
    now: u64,
) -> (String, u64) {
    let ct = media_type.unwrap_or("application/octet-stream");
    let token = signer.seal_object_target(&ObjectTokenTarget {
        cid: cid.to_string(),
        uri: uri.to_string(),
    });
    let (exp, sig) = signer.sign(&object_mac_id(&token), ct, now);
    let url = format!(
        "/v1/object/{token}/bytes?exp={exp}&ct={}&sig={sig}",
        pct(ct)
    );
    (url, exp.saturating_sub(now))
}

struct ServerObjectSigner {
    signer: Arc<AssetSigner>,
}

impl ObjectSigner for ServerObjectSigner {
    fn sign(&self, cid: &str, uri: &str, media_type: Option<&str>) -> Option<(String, u64)> {
        Some(mint_object_url(
            &self.signer,
            cid,
            uri,
            media_type,
            unix_now(),
        ))
    }
}

/// 调用者身份（由 API Key 解析）。
#[derive(Debug, Clone)]
pub struct Principal {
    pub tenant: Option<String>,
    pub tags: Vec<String>,
}

/// 检索延迟直方图桶上界（秒，升序）。
const LAT_BUCKETS: [f64; 11] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    searches: AtomicU64,
    indexed: AtomicU64,
    errors: AtomicU64,
    unauthorized: AtomicU64,
    rate_limited: AtomicU64,
    /// 累积桶计数：`lat_buckets[i]` = 延迟 ≤ `LAT_BUCKETS[i]` 的检索数。
    lat_buckets: [AtomicU64; LAT_BUCKETS.len()],
    lat_sum_micros: AtomicU64,
    lat_count: AtomicU64,
}

impl Metrics {
    /// 记录一次检索延迟（秒）到累积直方图。
    fn observe_search_latency(&self, secs: f64) {
        self.lat_count.fetch_add(1, Ordering::Relaxed);
        self.lat_sum_micros
            .fetch_add((secs * 1e6) as u64, Ordering::Relaxed);
        for (i, ub) in LAT_BUCKETS.iter().enumerate() {
            if secs <= *ub {
                self.lat_buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// 令牌桶（按调用方 key 分桶）。`check` 同步、无 await，故用 std Mutex。
struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

/// 简单令牌桶限流：每个 key 一桶，容量 `capacity`、每秒回填 `refill_per_sec`。
pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: std::sync::Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// 取 1 个令牌；足够则放行并返回 true，否则 false（限流）。
    fn check(&self, key: &str) -> bool {
        let now = std::time::Instant::now();
        let mut map = self.buckets.lock().unwrap();
        let b = map.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// 一条审计事件（谁/在哪个入口/查了什么/命中多少/结果状态）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEvent {
    pub endpoint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hits: Option<usize>,
    pub status: u16,
}

/// 审计 sink：服务把每条 [`AuditEvent`] 交给它（落 stderr / 日志系统 / 测试捕获）。
pub type AuditSink = Arc<dyn Fn(AuditEvent) + Send + Sync>;

/// 服务状态（可 Clone：内部 Arc 共享）。
#[derive(Clone)]
pub struct ServerState {
    engine: Arc<Mutex<Engine>>,
    keys: Arc<HashMap<String, Principal>>,
    metrics: Arc<Metrics>,
    rate_limiter: Option<Arc<RateLimiter>>,
    audit: Option<AuditSink>,
    /// 真语义嵌入后端（None=不嵌入，检索退化为 keyword）。
    embedder: Option<Arc<dyn Embedder + Send + Sync>>,
    /// 资产 URL 签名器（None=不签发短时 URL，`/v1/asset/{cid}/bytes` 一律 403）。
    asset_signer: Option<Arc<AssetSigner>>,
    /// 公网入口 base（用于在 `media.url` 中拼出完整 URL）；
    /// 与 `asset_signer` 共同决定 search hit 是否带 `media.url`。
    public_base: Option<String>,
    /// **集合注册表**（咨询性、内存态、可重建）：记录集合期望的 `dim`/`distance`，供 ingest 维度
    /// 校验 + introspection。**不是真源、不跨副本共享**（多副本各自一份）；向量后端是**服务端级**
    /// 选择（`FASTSEARCH_VECTOR_BACKEND` env），不在此处按集合实例化。见 [docs/benchmark-strategy.md]。
    collections: Arc<Mutex<HashMap<String, CollectionSpec>>>,
}

/// 集合的咨询性配置（注册表项）。`dim` 设了则 ingest 校验预计算向量维度一致；`distance` 仅记录。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CollectionSpec {
    /// 期望向量维度（None=不校验，首条预计算向量隐含确定）。
    #[serde(default)]
    pub dim: Option<usize>,
    /// 距离度量（信息性：`cosine`(默认)/`dot`/`l2`；引擎侧暴力档为余弦）。
    #[serde(default)]
    pub distance: Option<String>,
}

impl ServerState {
    pub fn new(engine: Engine, keys: HashMap<String, Principal>) -> Self {
        ServerState {
            engine: Arc::new(Mutex::new(engine)),
            keys: Arc::new(keys),
            metrics: Arc::new(Metrics::default()),
            rate_limiter: None,
            audit: None,
            embedder: None,
            asset_signer: None,
            public_base: None,
            collections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 配置资产 URL 签名器（MM6-signer）：开启后 `/v1/assets/resolve` 可签发短时 token URL、
    /// `/v1/asset/{cid}/bytes` 凭 token 取 inline 字节。`key` 为 HMAC 密钥（多副本须同值）。
    pub fn with_asset_signer(mut self, key: Vec<u8>, ttl_secs: u64) -> Self {
        self.asset_signer = Some(Arc::new(AssetSigner::new(key, ttl_secs.max(1))));
        self
    }

    /// 配置公网入口 base：`search`/`similar` 命中附带 `media.url` 时拼前缀（去掉末尾 `/`）。
    /// 配 signer 后才生效；二者缺一 → `media.url` 字段不出。
    pub fn with_public_base(mut self, base: impl Into<String>) -> Self {
        let s = base.into();
        self.public_base = Some(s.trim_end_matches('/').to_string());
        self
    }

    /// 让 engine 的 Object resolve 分支使用同一 HMAC 签发 object byte token URL。
    pub async fn enable_object_url_signer(self) -> Self {
        if let Some(signer) = &self.asset_signer {
            self.engine
                .lock()
                .await
                .set_object_signer(Box::new(ServerObjectSigner {
                    signer: signer.clone(),
                }));
        }
        self
    }

    /// 设置嵌入后端：ingest 自动嵌入 passage、search 自动嵌入 query（开启真混合）。
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder + Send + Sync>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// 在引擎锁外、`spawn_blocking` 里算嵌入（HTTP 阻塞调用不卡 async 运行时、不持锁）。
    async fn embed(&self, texts: Vec<String>, kind: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
        let Some(emb) = self.embedder.clone() else {
            return Ok(vec![]);
        };
        tokio::task::spawn_blocking(move || emb.embed(&texts, kind))
            .await
            .map_err(|e| format!("embed task join: {e}"))?
            .map_err(|e| format!("embed: {e}"))
    }

    async fn embed_inputs(
        &self,
        inputs: Vec<EmbedInput>,
        kind: EmbedKind,
    ) -> Result<Vec<Vec<f32>>, String> {
        let Some(emb) = self.embedder.clone() else {
            return Ok(vec![]);
        };
        tokio::task::spawn_blocking(move || emb.embed_multi(&inputs, kind))
            .await
            .map_err(|e| format!("embed task join: {e}"))?
            .map_err(|e| format!("embed: {e}"))
    }

    /// 开启限流（每 key 令牌桶：容量 + 每秒回填）。
    pub fn with_rate_limit(mut self, capacity: f64, refill_per_sec: f64) -> Self {
        self.rate_limiter = Some(Arc::new(RateLimiter {
            capacity,
            refill_per_sec,
            buckets: std::sync::Mutex::new(HashMap::new()),
        }));
        self
    }

    /// 设置审计 sink（每个成功请求发一条 [`AuditEvent`]）。
    pub fn with_audit(mut self, sink: AuditSink) -> Self {
        self.audit = Some(sink);
        self
    }

    /// 限流判定：放行 true，限流 false（并计数）。无限流器时恒放行。
    fn allow(&self, key: &str) -> bool {
        match &self.rate_limiter {
            Some(rl) if !rl.check(key) => {
                self.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => true,
        }
    }

    fn emit_audit(&self, ev: AuditEvent) {
        if let Some(sink) = &self.audit {
            sink(ev);
        }
    }

    /// 启动**后台 CDC 同步循环**：每 `interval` 拍调一次 `engine.consume_once`
    /// （peek→应用→落盘→advance，崩溃安全）。slot 位置由 PG 服务端持久，无需传 LSN。
    /// 注意：consume 期间持有引擎锁（与检索串行）——v1 可接受，低延迟化待引擎并发优化。
    /// 嵌入由引擎自身的 embedder 负责（需在建 state 前 `engine.set_embedder`）。
    pub fn spawn_cdc(
        &self,
        cfg: ReplicationConfig,
        data_dir: std::path::PathBuf,
        interval: std::time::Duration,
    ) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut e = engine.lock().await;
                    match e.consume_once(&cfg, &data_dir).await {
                        Ok(n) if n > 0 => eprintln!("cdc: applied {n} change(s)"),
                        Ok(_) => {}
                        Err(err) => eprintln!("cdc error: {err}"),
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }
}

/// 从请求头取原始 key 字符串（用于限流分桶；无则 `"anon"`）。
fn rate_key(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "anon".to_string())
}

/// 从请求头解析 Principal：`Authorization: Bearer <k>` 或 `X-API-Key: <k>`。纯函数。
pub fn principal_from_headers(
    headers: &HeaderMap,
    keys: &HashMap<String, Principal>,
) -> Option<Principal> {
    let key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })?;
    keys.get(key.trim()).cloned()
}

/// 鉴权门：解析 Principal，失败则记 `unauthorized` 指标并返回 401。供无 ACL 注入需求的端点
/// （集合注册/introspection）复用，避免重复样板。
fn require_principal(
    s: &ServerState,
    headers: &HeaderMap,
) -> Result<Principal, (StatusCode, String)> {
    principal_from_headers(headers, &s.keys).ok_or_else(|| {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key".to_string(),
        )
    })
}

/// Principal → 强制 ACL 过滤（服务端注入，客户端不可绕过）。纯函数。
pub fn acl_for(p: &Principal) -> AclFilter {
    AclFilter {
        tenant: p.tenant.clone(),
        allowed_tags: p.tags.clone(),
    }
}

/// 构建 router。
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ready" }))
        .route("/metrics", get(metrics))
        .route("/openapi.json", get(openapi))
        .route("/v1/search", post(search))
        .route("/v1/similar", post(similar))
        .route("/v1/asset/{cid}", get(asset))
        .route("/v1/asset/{cid}/bytes", get(asset_bytes))
        .route("/v1/object/{opaque}/bytes", get(object_bytes))
        .route("/v1/assets/resolve", post(assets_resolve))
        .route("/v1/index", post(index))
        .route("/v1/images", post(image_upload))
        .route("/v1/docs/{collection}/{*doc_id}", delete(delete_doc))
        .route(
            "/v1/collections",
            post(create_collection).get(list_collections),
        )
        .route("/v1/collections/{name}", get(get_collection))
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024))
        .with_state(state)
}

/// OpenAPI 3.0 契约（手写、随 API 演进维护）。供 SDK 生成 / 文档 / 契约校验（F54）。
async fn openapi() -> Json<Value> {
    Json(openapi_spec())
}

fn openapi_spec() -> Value {
    let api_key = json!({ "type": "apiKey", "in": "header", "name": "X-API-Key" });
    let hit = json!({
        "type": "object",
        "properties": {
            "citation_id": {"type": "string", "description": "collection:doc_id:chunk_id"},
            "score": {"type": "number"},
            "bm25": {"type": ["number", "null"]},
            "vector": {"type": ["number", "null"]},
            "rerank": {"type": ["number", "null"]},
            "doc_id": {"type": "string"},
            "chunk_id": {"type": "integer"},
            "page": {"type": "integer"},
            "bbox": {"type": "object"},
            "heading_path": {"type": "array", "items": {"type": "string"}},
            "section_id": {"type": "integer"},
            "highlight": {"type": ["string", "null"]},
            "merged_chunk_ids": {"type": "array", "items": {"type": "integer"}},
            "cursor": {"type": "string", "description": "深分页游标；作下次 search_after 续取下一页"}
        }
    });
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "fastsearch REST API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "混合检索引擎 REST 接口。认证：X-API-Key 或 Authorization: Bearer。\
                ACL 由认证身份服务端注入，客户端不可绕过。"
        },
        "components": {
            "securitySchemes": { "ApiKeyAuth": api_key },
            "schemas": {
                "SearchRequest": {
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {"type": "string"},
                        "mode": {"type": "string", "enum": ["keyword", "vector", "hybrid"], "default": "hybrid"},
                        "filter": {"type": ["object", "null"], "description": "core::Filter AST"},
                        "vector": {"type": ["array", "null"], "items": {"type": "number"}},
                        "query_image_base64": {"type": ["string", "null"], "description": "图片 query 的 base64 字节；服务端解码为内部 query_image"},
                        "candidates": {"type": "integer", "default": 150},
                        "ef_search": {"type": ["integer", "null"], "description":
                            "HNSW 检索期探索宽度逐查询覆盖（越大召回越高越慢；暴力/pgvector 档忽略）"},
                        "top_k": {"type": "integer", "default": 20},
                        "rerank": {"type": ["object", "null"]},
                        "auto_merge": {"type": "boolean", "default": false},
                        "collapse": {"type": ["object", "null"], "description": "{field, max_per_group}"},
                        "search_after": {"type": ["string", "null"], "description": "深分页游标（取自上一页末条命中的 cursor）"},
                        "highlight": {"type": "boolean", "default": false},
                        "facets": {"type": "array", "items": {"type": "string"}}
                    }
                },
                "Hit": hit,
                "IndexRequest": {
                    "type": "object",
                    "required": ["collection", "doc_id", "chunks"],
                    "properties": {
                        "collection": {"type": "string"},
                        "doc_id": {"type": "string"},
                        "store_media": {"type": ["string", "null"], "enum": ["inline", "auto", "object", "reference", null],
                            "description": "媒资存储策略：object/auto 上传 media_bytes 到对象存储；reference 校验已有 Object 引用"},
                        "chunks": {"type": "array", "items": {"type": "object",
                            "description": "core::Chunk 字段；可附 `vector`(number[]) 携带预计算向量，\
                                则跳过服务端嵌入直接入向量索引（benchmark/外部 ETL 用）"}}
                    }
                }
            }
        },
        "security": [{ "ApiKeyAuth": [] }],
        "paths": {
            "/v1/search": {
                "post": {
                    "summary": "混合检索",
                    "requestBody": {"required": true, "content": {
                        "application/json": {"schema": {"$ref": "#/components/schemas/SearchRequest"}},
                        "multipart/form-data": {"schema": {"type": "object", "properties": {
                            "request": {"type": "string", "description": "SearchRequest JSON 字符串，不含 query_image"},
                            "image": {"type": "string", "format": "binary"}
                        }}}
                    }},
                    "responses": {
                        "200": {"description": "命中 + 分面", "content": {"application/json": {"schema": {"type": "object",
                            "properties": {
                                "hits": {"type": "array", "items": {"$ref": "#/components/schemas/Hit"}},
                                "facets": {"type": "object"}
                            }}}}},
                        "401": {"description": "认证失败"},
                        "429": {"description": "限流"}
                    }
                }
            },
            "/v1/index": {
                "post": {
                    "summary": "灌入一个 doc 的 chunks（doc 级替换）",
                    "requestBody": {"required": true, "content": {"application/json":
                        {"schema": {"$ref": "#/components/schemas/IndexRequest"}}}},
                    "responses": {"200": {"description": "{indexed: n}"},
                        "400": {"description": "维度不符（集合已注册 dim 且预计算向量等维不符）"},
                        "401": {"description": "认证失败"}}
                }
            },
            "/v1/images": {
                "post": {
                    "summary": "上传单张原始图片文件并索引为 image chunk",
                    "description": "multipart 上传：server 负责对象存储、图像/跨模态嵌入、索引和 PG 真源写入。默认 store_media=object。",
                    "requestBody": {"required": true, "content": {"multipart/form-data":
                        {"schema": {"type": "object", "required": ["collection", "doc_id", "image"], "properties": {
                            "collection": {"type": "string"},
                            "doc_id": {"type": "string"},
                            "image": {"type": "string", "format": "binary"},
                            "text": {"type": "string", "description": "caption/OCR/描述文本；可为空"},
                            "page": {"type": "integer", "default": 1},
                            "store_media": {"type": "string", "enum": ["inline", "auto", "object", "reference"], "default": "object"}
                        }}}}},
                    "responses": {"200": {"description": "{indexed: 1}"},
                        "400": {"description": "缺少字段或 multipart 非法"},
                        "401": {"description": "认证失败"}}
                }
            },
            "/v1/docs/{collection}/{doc_id}": {
                "delete": {
                    "summary": "删除一个文档（PG 真源 + 派生索引 + 托管对象清理）",
                    "parameters": [
                        {"name": "collection", "in": "path", "required": true, "schema": {"type": "string"}},
                        {"name": "doc_id", "in": "path", "required": true, "schema": {"type": "string"}}
                    ],
                    "responses": {"200": {"description": "{deleted:true, pg_deleted, objects_deleted, object_errors}"},
                        "401": {"description": "认证失败"},
                        "404": {"description": "不可见或不存在"}}
                }
            },
            "/v1/collections": {
                "post": {
                    "summary": "注册/更新集合的咨询配置（dim/distance）。回显 + 服务端实际向量后端",
                    "description": "咨询性、内存态、不跨副本共享。向量后端是服务端级 env 选择，\
                        不在此按集合实例化。",
                    "requestBody": {"required": true, "content": {"application/json":
                        {"schema": {"type": "object", "required": ["name"], "properties": {
                            "name": {"type": "string"},
                            "dim": {"type": ["integer", "null"]},
                            "distance": {"type": ["string", "null"], "enum": ["cosine", "dot", "l2"]}}}}}},
                    "responses": {"200": {"description": "{name, dim, distance, server:{vector_backend,...}}"},
                        "401": {"description": "认证失败"}}
                },
                "get": {"summary": "列出已注册集合 + 服务端向量配置",
                    "responses": {"200": {"description": "{collections:[..], server:{..}}"},
                        "401": {"description": "认证失败"}}}
            },
            "/v1/collections/{name}": {
                "get": {"summary": "读回集合咨询配置 + 服务端实际向量配置（introspection）",
                    "responses": {"200": {"description": "集合配置"},
                        "404": {"description": "未注册"}, "401": {"description": "认证失败"}}}
            },
            "/v1/similar": {
                "post": {
                    "summary": "more_like_this：按 citation_id 反查相似",
                    "requestBody": {"required": true, "content": {"application/json":
                        {"schema": {"type": "object", "required": ["citation_id"], "properties": {
                            "citation_id": {"type": "string"}, "top_k": {"type": "integer", "default": 10}}}}}},
                    "responses": {"200": {"description": "命中列表"}, "400": {"description": "非法 citation_id"},
                        "401": {"description": "认证失败"}}
                }
            },
            "/v1/asset/{citation_id}": {
                "get": {
                    "summary": "媒资 ACL 网关（resolve_citation；不可见/不存在均 404）",
                    "responses": {
                        "200": {"description": "inline 字节（按需从 PG 真源取）/ DocRender JSON（跳原文 page+bbox）"},
                        "302": {"description": "SignedUrl 重定向到短时签名 URL"},
                        "401": {"description": "认证失败"},
                        "404": {"description": "不可见或不存在"}
                    }
                }
            },
            "/v1/assets/resolve": {
                "post": {
                    "summary": "批量把 citation_id 解析成可直接用的短时 URL（前端 <img src>）；ACL 强制，越权 id 省略",
                    "requestBody": {"required": true, "content": {"application/json":
                        {"schema": {"type": "object", "required": ["ids"], "properties": {
                            "ids": {"type": "array", "items": {"type": "string"}}}}}}},
                    "responses": {"200": {"description": "{assets:[{citation_id,type:inline|object|doc_render,url?,expires_s?,...}]}"},
                        "401": {"description": "认证失败"}}
                }
            },
            "/v1/asset/{citation_id}/bytes": {
                "get": {
                    "summary": "token 门控 inline 字节（HMAC 签名 URL，免 Bearer；让 <img src> 直取）",
                    "security": [],
                    "parameters": [
                        {"name": "exp", "in": "query", "required": true, "schema": {"type": "integer"}},
                        {"name": "ct", "in": "query", "required": true, "schema": {"type": "string"}},
                        {"name": "sig", "in": "query", "required": true, "schema": {"type": "string"}},
                        {"name": "Range", "in": "header", "required": false, "schema": {"type": "string"}, "description": "bytes=A-B / A- / -N（单段；音视频 seek/断点续传）"}
                    ],
                    "responses": {
                        "200": {"description": "inline 全量字节 + Content-Type=ct + Accept-Ranges: bytes"},
                        "206": {"description": "Partial Content：单段 Range 命中，带 Content-Range: bytes A-B/total"},
                        "403": {"description": "未配签名器 / token 无效或过期"},
                        "404": {"description": "无字节"},
                        "416": {"description": "Range 不可满足（起点越界），带 Content-Range: bytes */total"}
                    }
                }
            },
            "/v1/object/{opaque}/bytes": {
                "get": {
                    "summary": "token 门控 Object 字节（HMAC 签名 URL，免 Bearer；让 <img src> 直取）",
                    "security": [],
                    "parameters": [
                        {"name": "opaque", "in": "path", "required": true, "schema": {"type": "string"}},
                        {"name": "exp", "in": "query", "required": true, "schema": {"type": "integer"}},
                        {"name": "ct", "in": "query", "required": true, "schema": {"type": "string"}},
                        {"name": "sig", "in": "query", "required": true, "schema": {"type": "string"}},
                        {"name": "Range", "in": "header", "required": false, "schema": {"type": "string"}}
                    ],
                    "responses": {
                        "200": {"description": "Object 全量字节 + Content-Type=ct + Accept-Ranges: bytes"},
                        "206": {"description": "Partial Content"},
                        "403": {"description": "未配签名器 / token 无效或过期"},
                        "404": {"description": "无对象或对象存储未配置"},
                        "416": {"description": "Range 不可满足"}
                    }
                }
            },
            "/healthz": {"get": {"summary": "存活探针", "security": [], "responses": {"200": {"description": "ok"}}}},
            "/readyz": {"get": {"summary": "就绪探针", "security": [], "responses": {"200": {"description": "ready"}}}},
            "/metrics": {"get": {"summary": "Prometheus 指标", "security": [], "responses": {"200": {"description": "text/plain"}}}}
        }
    })
}

async fn metrics(State(s): State<ServerState>) -> String {
    let m = &s.metrics;
    let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let mut out = String::new();
    let counter = |out: &mut String, name: &str, help: &str, v: u64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"
        ));
    };
    counter(
        &mut out,
        "fastsearch_requests_total",
        "Total HTTP requests handled.",
        g(&m.requests),
    );
    counter(
        &mut out,
        "fastsearch_searches_total",
        "Total successful searches.",
        g(&m.searches),
    );
    counter(
        &mut out,
        "fastsearch_indexed_total",
        "Total chunks indexed.",
        g(&m.indexed),
    );
    counter(
        &mut out,
        "fastsearch_errors_total",
        "Total requests answered with 5xx.",
        g(&m.errors),
    );
    counter(
        &mut out,
        "fastsearch_unauthorized_total",
        "Total requests rejected for auth.",
        g(&m.unauthorized),
    );
    counter(
        &mut out,
        "fastsearch_rate_limited_total",
        "Total requests rejected by rate limit.",
        g(&m.rate_limited),
    );

    // 检索延迟直方图（Prometheus 累积 le 桶）。
    out.push_str(
        "# HELP fastsearch_search_latency_seconds Search latency in seconds.\n\
         # TYPE fastsearch_search_latency_seconds histogram\n",
    );
    for (i, ub) in LAT_BUCKETS.iter().enumerate() {
        out.push_str(&format!(
            "fastsearch_search_latency_seconds_bucket{{le=\"{ub}\"}} {}\n",
            g(&m.lat_buckets[i])
        ));
    }
    let count = g(&m.lat_count);
    out.push_str(&format!(
        "fastsearch_search_latency_seconds_bucket{{le=\"+Inf\"}} {count}\n"
    ));
    out.push_str(&format!(
        "fastsearch_search_latency_seconds_sum {}\n",
        g(&m.lat_sum_micros) as f64 / 1e6
    ));
    out.push_str(&format!(
        "fastsearch_search_latency_seconds_count {count}\n"
    ));
    out
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn public_media(media: &Option<fastsearch_core::MediaRef>) -> Option<PublicMediaRef> {
    media.as_ref().map(|m| m.to_public())
}

fn filter_targets_image(filter: Option<&Filter>) -> bool {
    match filter {
        Some(Filter::Eq(field, FieldValue::Str(v))) => field == "modality" && v == "image",
        Some(Filter::In(field, vals)) if field == "modality" => vals
            .iter()
            .any(|v| matches!(v, FieldValue::Str(s) if s == "image")),
        Some(Filter::And(parts)) | Some(Filter::Or(parts)) => {
            parts.iter().any(|f| filter_targets_image(Some(f)))
        }
        Some(Filter::Not(_)) | Some(Filter::Ne(_, _)) => false,
        _ => false,
    }
}

/// 命中列表 → JSON 数组（search / similar 共用）。
/// `signer` + `public_base` 都设了才会签 `media.url`，否则只吐 `media.asset`（脱敏）。
fn hits_json(
    hits: &[fastsearch_engine::SearchHit],
    signer: Option<&AssetSigner>,
    public_base: Option<&str>,
) -> Vec<Value> {
    let now = unix_now();
    hits.iter()
        .map(|h| {
            let media_pub = public_media(&h.citation.media);
            // 给命中拼 `media.url`：Inline → /v1/asset/.../bytes；Object → /v1/object/.../bytes；
            // DocRegion → 无字节，url=null。签发时已过 ACL，验签端点免 Bearer。
            let media_url = match (
                &h.citation.media,
                signer,
                public_base,
                &h.citation.citation_id(),
            ) {
                (Some(m), Some(sg), Some(base), cid) => match &m.asset {
                    AssetPointer::Inline => {
                        let (path, _exp_s) = mint_inline_url(sg, cid, m.media_type.as_deref(), now);
                        Some(format!("{base}{path}"))
                    }
                    AssetPointer::Object { uri } => {
                        let (path, _exp_s) =
                            mint_object_url(sg, cid, uri, m.media_type.as_deref(), now);
                        Some(format!("{base}{path}"))
                    }
                    AssetPointer::DocRegion { .. } => None,
                },
                _ => None,
            };
            // 在 public MediaRef 上挂 `url`（其余字段已被 `to_public` 脱敏）
            let media_json = match media_pub {
                Some(m) => {
                    let mut obj = serde_json::to_value(&m).unwrap_or(Value::Null);
                    if let (Some(u), Some(map)) = (media_url, obj.as_object_mut()) {
                        map.insert("url".into(), Value::String(u));
                    }
                    Some(obj)
                }
                None => None,
            };
            json!({
                "citation_id": h.citation.citation_id(),
                "score": h.score,
                "bm25": h.bm25,
                "vector": h.vector,
                "rerank": h.rerank,
                "doc_id": h.id.doc_id,
                "chunk_id": h.id.chunk_id,
                "page": h.citation.page,
                "bbox": h.citation.bbox,
                "heading_path": h.citation.heading_path,
                "section_id": h.citation.section_id,
                "highlight": h.highlight,
                "merged_chunk_ids": h.merged_chunk_ids,
                "time": h.citation.time,
                "media": media_json,
                // 深分页游标：把末条命中的此值作为下次请求的 search_after 即续取下一页。
                "cursor": h.cursor(),
            })
        })
        .collect()
}

fn decode_search_value(mut v: Value) -> Result<SearchRequest, (StatusCode, String)> {
    if v.get("query_image").is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "query_image is internal; use query_image_base64 or multipart image".into(),
        ));
    }
    let query_image = v
        .as_object_mut()
        .and_then(|obj| obj.remove("query_image_base64"))
        .map(|raw| {
            raw.as_str()
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        "query_image_base64 must be a string".into(),
                    )
                })
                .and_then(|s| {
                    B64.decode(s).map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!("invalid query_image_base64: {e}"),
                        )
                    })
                })
        })
        .transpose()?;
    let mut req: SearchRequest = serde_json::from_value(v).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid search request: {e}"),
        )
    })?;
    req.query_image = query_image;
    Ok(req)
}

async fn search(State(s): State<ServerState>, headers: HeaderMap, req: Request<Body>) -> ApiResult {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let req = if content_type.starts_with("multipart/form-data") {
        let mut mp = Multipart::from_request(req, &s).await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid multipart body: {e}"),
            )
        })?;
        let mut request_json: Option<Value> = None;
        let mut image: Option<Vec<u8>> = None;
        while let Some(field) = mp.next_field().await.map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid multipart field: {e}"),
            )
        })? {
            match field.name().unwrap_or_default() {
                "request" => {
                    let text = field.text().await.map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!("invalid request field: {e}"),
                        )
                    })?;
                    request_json = Some(serde_json::from_str(&text).map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!("invalid request json: {e}"),
                        )
                    })?);
                }
                "image" => {
                    let bytes = field.bytes().await.map_err(|e| {
                        (StatusCode::BAD_REQUEST, format!("invalid image field: {e}"))
                    })?;
                    image = Some(bytes.to_vec());
                }
                _ => {}
            }
        }
        let mut req = decode_search_value(request_json.unwrap_or_else(|| json!({"query": ""})))?;
        if image.is_some() {
            req.query_image = image;
        }
        req
    } else {
        let bytes = to_bytes(req.into_body(), 20 * 1024 * 1024)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid body: {e}")))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid json: {e}")))?;
        decode_search_value(v)?
    };
    search_request(s, headers, req).await
}

async fn search_request(s: ServerState, headers: HeaderMap, req: SearchRequest) -> ApiResult {
    let started = std::time::Instant::now();
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()));
    }
    let principal = principal_from_headers(&headers, &s.keys).ok_or_else(|| {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key".into(),
        )
    })?;
    let acl = acl_for(&principal);

    // 真混合：mode 需要向量、客户端未传 vector、且配了嵌入后端 → 锁外嵌入 query。
    // **带 `query_image`（以图搜图，MM9）时不预嵌文本**——否则会用空文本向量遮蔽图像查询；
    // 留 `vector=None` 交引擎 `embed_query_image` 嵌图。
    let mut req = req;
    let needs_vec = matches!(req.mode, SearchMode::Hybrid | SearchMode::Vector);
    let text_to_image = req.query_image.is_none()
        && !req.query.trim().is_empty()
        && filter_targets_image(req.filter.as_ref());
    let cross_modal_ok = s
        .embedder
        .as_ref()
        .is_none_or(|emb| !text_to_image || emb.caps().cross_modal);
    if needs_vec
        && req.vector.is_none()
        && req.query_image.is_none()
        && s.embedder.is_some()
        && cross_modal_ok
    {
        match s.embed(vec![req.query.clone()], EmbedKind::Query).await {
            Ok(mut v) if !v.is_empty() => req.vector = Some(v.remove(0)),
            Ok(_) => {}
            Err(e) => {
                s.metrics.errors.fetch_add(1, Ordering::Relaxed);
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
            }
        }
    }

    let engine = s.engine.lock().await;
    let (hits, facets) = engine
        .search_with_facets(&req, Some(&acl)) // ACL 强制注入，客户端不可绕过
        .map_err(|e| {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    s.metrics.searches.fetch_add(1, Ordering::Relaxed);
    s.metrics
        .observe_search_latency(started.elapsed().as_secs_f64());

    let arr = hits_json(&hits, s.asset_signer.as_deref(), s.public_base.as_deref());
    // 分面 → {field: [{value, count}]}
    let facets_json: Value = facets
        .into_iter()
        .map(|(field, pairs)| {
            let vals: Vec<Value> = pairs
                .into_iter()
                .map(|(v, c)| json!({ "value": v, "count": c }))
                .collect();
            (field, Value::Array(vals))
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    s.emit_audit(AuditEvent {
        endpoint: "/v1/search",
        tenant: principal.tenant.clone(),
        tags: principal.tags.clone(),
        query: Some(req.query.clone()),
        collection: None,
        doc_id: None,
        hits: Some(arr.len()),
        status: 200,
    });
    Ok(Json(json!({ "hits": arr, "facets": facets_json })))
}

#[derive(Deserialize)]
struct SimilarBody {
    citation_id: String,
    #[serde(default = "default_similar_k")]
    top_k: usize,
}
fn default_similar_k() -> usize {
    10
}

/// more_like_this：按种子 citation_id 反查相似命中（ACL 强制注入，不可绕过）。
async fn similar(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<SimilarBody>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()));
    }
    let principal = principal_from_headers(&headers, &s.keys).ok_or_else(|| {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key".into(),
        )
    })?;
    let acl = acl_for(&principal);
    let gid =
        GlobalId::parse(&body.citation_id).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let engine = s.engine.lock().await;
    let hits = engine
        .more_like_this(&gid, body.top_k, Some(&acl))
        .map_err(|e| {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    s.metrics.searches.fetch_add(1, Ordering::Relaxed);
    s.emit_audit(AuditEvent {
        endpoint: "/v1/similar",
        tenant: principal.tenant.clone(),
        tags: principal.tags.clone(),
        query: Some(body.citation_id.clone()),
        collection: None,
        doc_id: None,
        hits: Some(hits.len()),
        status: 200,
    });
    Ok(Json(
        json!({ "hits": hits_json(&hits, s.asset_signer.as_deref(), s.public_base.as_deref()) }),
    ))
}

/// 单段 `Range: bytes=…` 头的解析结果。多段（含逗号）不支持 → `None`（退 200 全量，RFC 7233
/// 允许服务端忽略 Range）。
enum RangeSpec {
    /// 无 Range 头或语法非法/不支持 → 返回 200 全量。
    None,
    /// 闭区间 `[start, end]`（0-based、含端，已对总长截断）→ 206 Partial Content。
    Range(u64, u64),
    /// 语法可解析但不可满足（起点越界 / 后缀 0 / 空体）→ 416。
    Unsatisfiable,
}

/// 解析 `Range: bytes=A-B`（支持 `A-`、`-N` 后缀式）为闭区间。仅支持单段；`total` 为资源总字节。
fn parse_range(headers: &HeaderMap, total: u64) -> RangeSpec {
    let Some(spec) = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .map(str::trim)
    else {
        return RangeSpec::None;
    };
    // 多段 byte-range 不支持 → 忽略 Range、退回 200 全量。
    if spec.contains(',') {
        return RangeSpec::None;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return RangeSpec::None; // 语法非法 → 当作无 Range
    };
    let (start, end) = if a.is_empty() {
        // 后缀式 `-N`：末尾 N 字节。
        let Ok(n) = b.parse::<u64>() else {
            return RangeSpec::None;
        };
        if n == 0 || total == 0 {
            return RangeSpec::Unsatisfiable;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let Ok(start) = a.parse::<u64>() else {
            return RangeSpec::None;
        };
        let end = if b.is_empty() {
            total.saturating_sub(1)
        } else {
            match b.parse::<u64>() {
                Ok(e) => e.min(total.saturating_sub(1)),
                Err(_) => return RangeSpec::None,
            }
        };
        (start, end)
    };
    if total == 0 || start >= total || start > end {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Range(start, end)
}

/// 把 inline 字节按 `Range` 头组装响应：无 Range→200 全量 + `Accept-Ranges: bytes`；
/// 单段→206 + `Content-Range`；不可满足→416 + `Content-Range: bytes */total`。两个 inline
/// 出口（authed `asset` / token 门控 `asset_bytes`）共用，确保 Range 语义一致。
fn serve_inline_bytes(headers: &HeaderMap, ct: String, bytes: Vec<u8>) -> Response {
    let total = bytes.len() as u64;
    match parse_range(headers, total) {
        RangeSpec::None => (
            [
                (header::CONTENT_TYPE, ct),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            bytes,
        )
            .into_response(),
        RangeSpec::Range(start, end) => {
            let slice = bytes[start as usize..=end as usize].to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ct),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    ),
                ],
                slice,
            )
                .into_response()
        }
        RangeSpec::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            "range not satisfiable",
        )
            .into_response(),
    }
}

/// 媒资 ACL 网关：`GET /v1/asset/{citation_id}` —— `principal→acl_for→resolve_citation`，
/// ACL 不可绕过（不可见/不存在均 404，不暴露存在性）。InlineRef 按需从 PG 真源取字节直吐（MM6-inline）、
/// SignedUrl 302（由 `ObjectSigner` 签短时 URL；**未配签名器时 Object→404，绝不暴露裸 key**，MM6-secure）、
/// DocRender 返回跳原文 JSON。inline 字节支持 `Range`（音视频 seek / 断点续传）；对象存储档 Range
/// 由签名 URL 转交对象存储处理。
async fn asset(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Path(cid): Path<String>,
) -> Response {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    let Some(principal) = principal_from_headers(&headers, &s.keys) else {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::UNAUTHORIZED, "missing or invalid API key").into_response();
    };
    let acl = acl_for(&principal);
    let engine = s.engine.lock().await;
    let resolved = match engine.resolve_citation(&cid, Some(&acl)) {
        Ok(r) => r,
        Err(e) => {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    // engine 保持锁定到 InlineRef 取字节后（resolve 已定位+ACL；inline 字节按需 fetch_inline_bytes）。
    let status = if resolved.is_some() { 200 } else { 404 };
    s.emit_audit(AuditEvent {
        endpoint: "/v1/asset",
        tenant: principal.tenant.clone(),
        tags: principal.tags.clone(),
        query: Some(cid.clone()),
        collection: None,
        doc_id: None,
        hits: None,
        status,
    });
    let Some(a) = resolved else {
        // 不可见或不存在——均 404，不暴露存在性。
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    match a.fetch {
        AssetFetch::DocRender { doc_id, page, bbox } => Json(json!({
            "type": "doc_render",
            "doc_id": doc_id,
            "page": page,
            "bbox": bbox,
            "time": a.time,
            "media_type": a.media_type,
        }))
        .into_response(),
        AssetFetch::SignedUrl { url, .. } => Redirect::temporary(&url).into_response(),
        // inline：resolve 已定位+ACL，按需从 PG 真源取字节（authed 出口，已授权）→ 吐字节 + Content-Type。
        AssetFetch::InlineRef => match engine.fetch_inline_bytes(&cid) {
            Ok(Some(bytes)) => {
                let ct = a
                    .media_type
                    .unwrap_or_else(|| "application/octet-stream".into());
                serve_inline_bytes(&headers, ct, bytes)
            }
            Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
            Err(e) => {
                s.metrics.errors.fetch_add(1, Ordering::Relaxed);
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        },
    }
}

/// `/v1/asset/{cid}/bytes` 的签名 token 查询参数。
#[derive(Deserialize)]
struct BytesQuery {
    exp: u64,
    ct: String,
    sig: String,
}

/// **token 门控** inline 字节端点（MM6-signer）：**不走 Bearer**，凭 `/v1/assets/resolve` 签发的
/// 短时 token 取字节——让前端 `<img src>` 免鉴权头直接渲染。验签（`HMAC(cid|exp|ct)` 常量时间 + 未
/// 过期）即授权（签名时已过 ACL，presigned 语义，守不变量 #3）；失败 403，不暴露原因细节。
/// 未配签名器 → 一律 403。验签通过后从 PG 真源取字节（`fetch_inline_bytes`，无字节→404）。
async fn asset_bytes(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Path(cid): Path<String>,
    Query(q): Query<BytesQuery>,
) -> Response {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    let Some(signer) = &s.asset_signer else {
        return (StatusCode::FORBIDDEN, "asset signing not configured").into_response();
    };
    if !signer.verify(&cid, q.exp, &q.ct, &q.sig, unix_now()) {
        return (StatusCode::FORBIDDEN, "invalid or expired token").into_response();
    }
    // 验签通过 = 已授权（签发时过的 ACL）→ 取 PG 真源字节，不再查 ACL。
    let engine = s.engine.lock().await;
    match engine.fetch_inline_bytes(&cid) {
        Ok(Some(bytes)) => serve_inline_bytes(&headers, q.ct, bytes),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Object 字节端点：`/v1/assets/resolve` 先过 ACL 并签 token；这里仅验 token 后从对象存储取字节。
async fn object_bytes(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Path(opaque): Path<String>,
    Query(q): Query<BytesQuery>,
) -> Response {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    }
    let Some(signer) = &s.asset_signer else {
        return (StatusCode::FORBIDDEN, "asset signing not configured").into_response();
    };
    if !object_token_encoded_len_ok(&opaque) {
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    }
    if !signer.verify(&object_mac_id(&opaque), q.exp, &q.ct, &q.sig, unix_now()) {
        return (StatusCode::FORBIDDEN, "invalid or expired token").into_response();
    }
    let Some(target) = signer.open_object_target(&opaque) else {
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    };
    let engine = s.engine.lock().await;
    // `cid` 来自本机 `seal_object_target` 序列化的合法 GlobalId；解析失败属服务端不变量违反
    // （token 被篡改但 MAC 通过 → HMAC 碰撞；或 token 版本不兼容）。返回 500 + 指标便于排查。
    let gid = match GlobalId::parse(&target.cid) {
        Ok(gid) => gid,
        Err(e) => {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "fastsearch-server: sealed token cid parse failed ({e}): {:?}",
                target.cid
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "sealed token cid unparseable",
            )
                .into_response();
        }
    };
    match engine.object_uri_for_gid(&gid) {
        Ok(Some(current)) if current == target.uri => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Ok(Some(_)) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    match engine.fetch_object_bytes(&target.uri, 20 * 1024 * 1024) {
        Ok(Some(obj)) => serve_inline_bytes(&headers, q.ct, obj.bytes),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct ResolveBody {
    ids: Vec<String>,
}

/// `POST /v1/assets/resolve`（authed）：批量把 citation_id 解析成**可直接用的短时 URL**（前端
/// `<img src>`）。每 id 经 `resolve_citation`（**ACL 服务端强制**）→ InlineRef 签短时 token URL、
/// Object 返签名 URL、DocRender 返跳原文 JSON。**越权/不存在的 id 直接省略**（不暴露存在性，守 #3）。
async fn assets_resolve(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<ResolveBody>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()));
    }
    let principal = principal_from_headers(&headers, &s.keys).ok_or_else(|| {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key".into(),
        )
    })?;
    let acl = acl_for(&principal);
    let now = unix_now();
    let engine = s.engine.lock().await;
    let mut out = Vec::new();
    for cid in &body.ids {
        let Some(a) = engine
            .resolve_citation(cid, Some(&acl))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            continue; // 越权/不存在 → 省略（不暴露存在性）
        };
        let item = match a.fetch {
            AssetFetch::InlineRef => match &s.asset_signer {
                Some(sig) => {
                    let (url, expires_s) = mint_inline_url(sig, cid, a.media_type.as_deref(), now);
                    json!({"citation_id": cid, "type": "inline", "url": url, "expires_s": expires_s, "media_type": a.media_type})
                }
                // 未配签名器：无法签 URL（不返回字节端点直链——它无 token 会 403）。
                None => {
                    json!({"citation_id": cid, "type": "inline", "error": "asset signing not configured"})
                }
            },
            AssetFetch::SignedUrl { url, expires_s } => {
                json!({"citation_id": cid, "type": "object", "url": url, "expires_s": expires_s})
            }
            AssetFetch::DocRender { doc_id, page, bbox } => {
                json!({"citation_id": cid, "type": "doc_render", "doc_id": doc_id, "page": page, "bbox": bbox, "media_type": a.media_type})
            }
        };
        out.push(item);
    }
    drop(engine);
    Ok(Json(json!({ "assets": out })))
}

/// 单个待索引 chunk：正常 chunk 字段（flatten）+ 可选**预计算向量**旁路。
/// 携带 `vector` 时直接入向量索引、**跳过服务端嵌入**（benchmark / 外部 ETL 已自带向量；
/// 见 [docs/benchmark-strategy.md]）；不带则照旧走嵌入后端（无后端→纯全文）。
#[derive(Deserialize)]
struct IndexChunk {
    #[serde(flatten)]
    chunk: Chunk,
    #[serde(default)]
    vector: Option<Vec<f32>>,
}

#[derive(Deserialize)]
struct IndexBody {
    collection: String,
    doc_id: String,
    #[serde(default)]
    store_media: StoreMedia,
    chunks: Vec<IndexChunk>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StoreMedia {
    #[default]
    Inline,
    Auto,
    Object,
    Reference,
}

async fn image_upload(
    State(s): State<ServerState>,
    headers: HeaderMap,
    req: Request<Body>,
) -> ApiResult {
    let principal = require_principal(&s, &headers)?;
    let mut mp = Multipart::from_request(req, &s).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid multipart body: {e}"),
        )
    })?;
    let mut collection: Option<String> = None;
    let mut doc_id: Option<String> = None;
    let mut text = String::new();
    let mut page: u32 = 1;
    let mut image: Option<(Vec<u8>, String)> = None;
    let mut store_media = StoreMedia::Object;

    while let Some(field) = mp.next_field().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid multipart field: {e}"),
        )
    })? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "image" => {
                let ct = field
                    .content_type()
                    .map(str::to_string)
                    .unwrap_or_else(|| "application/octet-stream".into());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid image field: {e}")))?;
                image = Some((bytes.to_vec(), ct));
            }
            "collection" => {
                collection = Some(field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("invalid collection field: {e}"),
                    )
                })?);
            }
            "doc_id" => {
                doc_id = Some(field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("invalid doc_id field: {e}"),
                    )
                })?);
            }
            "text" => {
                text = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid text field: {e}")))?;
            }
            "page" => {
                let raw = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid page field: {e}")))?;
                page = raw
                    .parse()
                    .map_err(|_| (StatusCode::BAD_REQUEST, "page must be u32".into()))?;
            }
            "store_media" => {
                let raw = field.text().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("invalid store_media field: {e}"),
                    )
                })?;
                store_media = match raw.as_str() {
                    "inline" => StoreMedia::Inline,
                    "auto" => StoreMedia::Auto,
                    "object" => StoreMedia::Object,
                    "reference" => StoreMedia::Reference,
                    _ => return Err((StatusCode::BAD_REQUEST, "invalid store_media".into())),
                };
            }
            _ => {}
        }
    }
    let collection =
        collection.ok_or_else(|| (StatusCode::BAD_REQUEST, "collection is required".into()))?;
    let doc_id = doc_id.ok_or_else(|| (StatusCode::BAD_REQUEST, "doc_id is required".into()))?;
    let (bytes, media_type) =
        image.ok_or_else(|| (StatusCode::BAD_REQUEST, "image is required".into()))?;
    let acl = ingest_acl_for(&principal);
    let chunk = Chunk {
        doc_id: doc_id.clone(),
        chunk_id: 1,
        kind: ChunkKind::Image,
        text,
        page,
        bbox: BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        },
        heading_path: vec![],
        section_id: 0,
        char_len: 0,
        media: Some(MediaRef {
            asset: AssetPointer::Inline,
            media_type: Some(media_type),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        }),
        media_bytes: Some(bytes),
        image_vector_status: Some(fastsearch_core::ImageVectorStatus::Pending),
        tenant: principal.tenant.clone(),
        acl,
    };
    index(
        State(s),
        headers,
        Json(IndexBody {
            collection,
            doc_id,
            store_media,
            chunks: vec![IndexChunk {
                chunk,
                vector: None,
            }],
        }),
    )
    .await
}

fn ext_for_media_type(media_type: Option<&str>) -> &'static str {
    match media_type.unwrap_or_default() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn short_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn object_namespace(tenant: Option<&str>) -> Result<String, (StatusCode, String)> {
    match tenant {
        Some(t)
            if !t.is_empty()
                && t != "."
                && t != ".."
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) =>
        {
            Ok(t.to_string())
        }
        Some(_) => Err((StatusCode::BAD_REQUEST, "invalid tenant namespace".into())),
        None => Ok("_global".into()),
    }
}

fn ingest_acl_for(principal: &Principal) -> Vec<String> {
    if principal.tags.is_empty() {
        vec!["public".into()]
    } else {
        principal.tags.clone()
    }
}

fn apply_ingest_identity(body: &mut IndexBody, principal: &Principal) {
    let acl = ingest_acl_for(principal);
    for ic in &mut body.chunks {
        ic.chunk.doc_id = body.doc_id.clone();
        ic.chunk.tenant = principal.tenant.clone();
        ic.chunk.acl = acl.clone();
    }
}

fn image_has_declared_bytes(chunk: &Chunk) -> bool {
    chunk.media_bytes.is_some()
        || chunk
            .media
            .as_ref()
            .is_some_and(|m| matches!(&m.asset, AssetPointer::Object { .. }))
}

fn initial_image_vector_status(ic: &IndexChunk) -> Option<fastsearch_core::ImageVectorStatus> {
    (ic.chunk.kind == ChunkKind::Image).then(|| {
        if ic.vector.is_some() {
            fastsearch_core::ImageVectorStatus::Embedded
        } else {
            ic.chunk
                .image_vector_status
                .unwrap_or(fastsearch_core::ImageVectorStatus::Pending)
        }
    })
}

async fn index(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(mut body): Json<IndexBody>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()));
    }
    let principal = principal_from_headers(&headers, &s.keys).ok_or_else(|| {
        s.metrics.unauthorized.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API key".into(),
        )
    })?;
    let err500 = |e: fastsearch_engine::EngineError| {
        s.metrics.errors.fetch_add(1, Ordering::Relaxed);
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    };
    apply_ingest_identity(&mut body, &principal);
    let namespace = object_namespace(principal.tenant.as_deref())?;
    let old_object_uris = {
        let engine = s.engine.lock().await;
        engine
            .object_uris_for_doc(&body.collection, &body.doc_id)
            .map_err(&err500)?
    };
    let mut new_object_uris = Vec::new();

    if matches!(body.store_media, StoreMedia::Auto | StoreMedia::Object) {
        let engine = s.engine.lock().await;
        for ic in &mut body.chunks {
            let Some(bytes) = ic.chunk.media_bytes.clone() else {
                continue;
            };
            let media_type = ic
                .chunk
                .media
                .as_ref()
                .and_then(|m| m.media_type.as_deref())
                .unwrap_or("application/octet-stream")
                .to_string();
            let key = format!(
                "{}/{}/{}/{}-{}.{}",
                namespace,
                body.collection,
                body.doc_id,
                ic.chunk.chunk_id,
                short_sha(&bytes),
                ext_for_media_type(Some(&media_type))
            );
            let obj = engine
                .put_object(&key, &bytes, &media_type)
                .map_err(&err500)?;
            new_object_uris.push(obj.uri.clone());
            let asset = AssetPointer::Object { uri: obj.uri };
            match &mut ic.chunk.media {
                Some(media) => media.asset = asset,
                None => {
                    ic.chunk.media = Some(MediaRef {
                        asset,
                        media_type: Some(media_type.clone()),
                        time: None,
                        region: None,
                        caption_source: None,
                        thumbnail: None,
                    });
                }
            }
            ic.chunk.media_bytes = None;
        }
    }
    {
        let engine = s.engine.lock().await;
        for ic in &body.chunks {
            let Some(AssetPointer::Object { uri }) = ic.chunk.media.as_ref().map(|m| &m.asset)
            else {
                continue;
            };
            engine
                .validate_object_ref(uri, principal.tenant.as_deref())
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        }
    }

    // 维度校验：集合若已注册 `dim`，则携带的预计算向量必须等维（benchmark/ETL 误配早失败，
    // 而非静默写入坏向量）。仅校验显式向量；嵌入产物维度由 embedder 保证一致。
    if let Some(dim) = s
        .collections
        .lock()
        .await
        .get(&body.collection)
        .and_then(|c| c.dim)
    {
        for ic in &body.chunks {
            if let Some(v) = &ic.vector {
                if v.len() != dim {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!(
                            "collection '{}' expects dim {dim}, chunk {} has {}",
                            body.collection,
                            ic.chunk.chunk_id,
                            v.len()
                        ),
                    ));
                }
            }
        }
    }

    // 每 chunk 一个向量槽：携带预计算向量者直接占位；其余锁外嵌入。
    // 图片 chunk 优先用 inline/object 字节；没有字节才退化为 caption/OCR 文本。
    // 无嵌入后端且无预计算向量 → 该槽留空、退化为纯全文。
    let mut vectors: Vec<Option<Vec<f32>>> = body.chunks.iter().map(|c| c.vector.clone()).collect();
    let mut image_statuses: Vec<Option<fastsearch_core::ImageVectorStatus>> = body
        .chunks
        .iter()
        .map(initial_image_vector_status)
        .collect();
    if let Some(embedder) = &s.embedder {
        let caps = embedder.caps();
        let mut need: Vec<(
            usize,
            EmbedInput,
            Option<fastsearch_core::ImageVectorStatus>,
        )> = Vec::new();
        for (i, ic) in body.chunks.iter().enumerate() {
            if ic.vector.is_some() {
                continue;
            }
            let chunk = &ic.chunk;
            let (input, status) = if chunk.kind == ChunkKind::Image {
                if caps.image && caps.cross_modal {
                    if let Some(bytes) = &chunk.media_bytes {
                        (
                            Some(EmbedInput::Image(bytes.clone())),
                            Some(fastsearch_core::ImageVectorStatus::Embedded),
                        )
                    } else if let Some(uri) = chunk.media.as_ref().and_then(|m| match &m.asset {
                        AssetPointer::Object { uri } => Some(uri.as_str()),
                        _ => None,
                    }) {
                        let engine = s.engine.lock().await;
                        match engine.fetch_object_bytes(uri, 20 * 1024 * 1024) {
                            Ok(Some(obj)) => (
                                Some(EmbedInput::Image(obj.bytes)),
                                Some(fastsearch_core::ImageVectorStatus::Embedded),
                            ),
                            Ok(None) if !chunk.text.trim().is_empty() => (
                                Some(EmbedInput::Text(chunk.text.clone())),
                                Some(fastsearch_core::ImageVectorStatus::TextFallback),
                            ),
                            Ok(None) => {
                                image_statuses[i] =
                                    Some(fastsearch_core::ImageVectorStatus::AssetMissing);
                                (None, None)
                            }
                            Err(e) => {
                                s.metrics.errors.fetch_add(1, Ordering::Relaxed);
                                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                            }
                        }
                    } else if !chunk.text.trim().is_empty() {
                        (
                            Some(EmbedInput::Text(chunk.text.clone())),
                            Some(fastsearch_core::ImageVectorStatus::TextFallback),
                        )
                    } else {
                        image_statuses[i] = Some(fastsearch_core::ImageVectorStatus::MissingBytes);
                        (None, None)
                    }
                } else if !chunk.text.trim().is_empty() {
                    (
                        Some(EmbedInput::Text(chunk.text.clone())),
                        Some(fastsearch_core::ImageVectorStatus::TextFallback),
                    )
                } else if image_has_declared_bytes(chunk) {
                    image_statuses[i] = Some(fastsearch_core::ImageVectorStatus::Pending);
                    (None, None)
                } else {
                    image_statuses[i] = Some(fastsearch_core::ImageVectorStatus::MissingBytes);
                    (None, None)
                }
            } else {
                (
                    (!chunk.text.trim().is_empty()).then(|| EmbedInput::Text(chunk.text.clone())),
                    None,
                )
            };
            if let Some(input) = input {
                need.push((i, input, status));
            }
        }
        if !need.is_empty() {
            let inputs: Vec<EmbedInput> = need.iter().map(|(_, input, _)| input.clone()).collect();
            let embedded = s
                .embed_inputs(inputs, EmbedKind::Passage)
                .await
                .map_err(|e| {
                    s.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::INTERNAL_SERVER_ERROR, e)
                })?;
            for ((i, _, status), v) in need.into_iter().zip(embedded) {
                vectors[i] = Some(v);
                if let Some(status) = status {
                    image_statuses[i] = Some(status);
                }
            }
        }
    }
    for (ic, status) in body.chunks.iter_mut().zip(image_statuses) {
        if ic.chunk.kind == ChunkKind::Image {
            ic.chunk.image_vector_status = status;
        }
    }

    let pg = {
        let engine = s.engine.lock().await;
        engine.source_pg_clone()
    };
    if let Some(pg_arc) = pg {
        let raw_chunks: Vec<fastsearch_core::Chunk> =
            body.chunks.iter().map(|ic| ic.chunk.clone()).collect();
        let upsert_err = pg_arc
            .upsert_doc(&body.collection, &body.doc_id, &raw_chunks)
            .await
            .err()
            .map(|e| format!("{e}"));
        if let Some(e) = upsert_err {
            s.metrics.errors.fetch_add(1, Ordering::Relaxed);
            let engine = s.engine.lock().await;
            for uri in &new_object_uris {
                if let Err(cleanup_err) = engine.delete_object(uri) {
                    eprintln!("object cleanup after pg upsert failure failed: {cleanup_err}");
                }
            }
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("pg upsert: {e}")));
        }
    }

    let mut engine = s.engine.lock().await;
    engine
        .remove_doc(&body.collection, &body.doc_id)
        .map_err(&err500)?;
    for (ic, v) in body.chunks.iter().zip(vectors) {
        match v {
            Some(v) => engine
                .ingest_vector(&body.collection, &ic.chunk, v)
                .map_err(&err500)?,
            None => engine
                .ingest(&body.collection, &ic.chunk)
                .map_err(&err500)?,
        }
    }
    engine.commit().map_err(&err500)?;
    for uri in old_object_uris
        .iter()
        .filter(|uri| !new_object_uris.iter().any(|new_uri| new_uri == *uri))
    {
        if let Err(e) = engine.delete_object(uri) {
            eprintln!("old object cleanup after doc replace failed: {e}");
        }
    }
    let n = body.chunks.len();
    s.metrics.indexed.fetch_add(n as u64, Ordering::Relaxed);
    s.emit_audit(AuditEvent {
        endpoint: "/v1/index",
        tenant: principal.tenant.clone(),
        tags: principal.tags.clone(),
        query: None,
        collection: Some(body.collection.clone()),
        doc_id: Some(body.doc_id.clone()),
        hits: Some(n),
        status: 200,
    });
    Ok(Json(json!({ "indexed": n })))
}

async fn delete_doc(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Path((collection, doc_id)): Path<(String, String)>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    if !s.allow(&rate_key(&headers)) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded".into()));
    }
    let principal = require_principal(&s, &headers)?;
    let acl = acl_for(&principal);

    let (object_uris, pg) = {
        let engine = s.engine.lock().await;
        let Some(visible) = engine
            .doc_visible_for_delete(&collection, &doc_id, Some(&acl))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        else {
            return Err((StatusCode::NOT_FOUND, "not found".into()));
        };
        if !visible {
            return Err((StatusCode::NOT_FOUND, "not found".into()));
        }
        (
            engine
                .object_uris_for_doc(&collection, &doc_id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
            engine.source_pg_clone(),
        )
    };

    let pg_deleted = if let Some(pg) = pg {
        pg.delete_doc(&collection, &doc_id)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("pg delete: {e}")))?
    } else {
        0
    };

    let mut object_errors = Vec::new();
    {
        let mut engine = s.engine.lock().await;
        engine
            .remove_doc(&collection, &doc_id)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        engine
            .commit()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        for uri in &object_uris {
            if let Err(e) = engine.delete_object(uri) {
                object_errors.push(e.to_string());
            }
        }
    }

    s.emit_audit(AuditEvent {
        endpoint: "/v1/docs/{collection}/{doc_id}",
        tenant: principal.tenant.clone(),
        tags: principal.tags.clone(),
        query: None,
        collection: Some(collection.clone()),
        doc_id: Some(doc_id.clone()),
        hits: Some(pg_deleted as usize),
        status: 200,
    });
    Ok(Json(json!({
        "deleted": true,
        "pg_deleted": pg_deleted,
        "objects_deleted": object_uris.len().saturating_sub(object_errors.len()),
        "object_errors": object_errors
    })))
}

/// `POST /v1/collections` 请求体：注册/更新集合的咨询性配置。
#[derive(Deserialize)]
struct CreateCollectionBody {
    name: String,
    #[serde(default)]
    dim: Option<usize>,
    #[serde(default)]
    distance: Option<String>,
}

/// 服务端实际向量配置（introspection，取自运行中的引擎；benchmark 据此确认"被测后端"）。
async fn server_vector_info(s: &ServerState) -> Value {
    let engine = s.engine.lock().await;
    json!({
        // pgvector 直查档下后端索引仍是底层暴力档，但召回在 PG → 用 "pgvector" 如实标注。
        "vector_backend": if engine.has_pg_vector() { "pgvector" } else { engine.vector_backend() },
        "vector_dim": engine.vector_dim(),
        "vector_count": engine.vector_len(),
        "embedded": s.embedder.is_some(),
    })
}

/// 注册/更新一个集合的咨询配置（幂等 upsert）。返回回显 + 服务端实际向量配置。
/// **不实例化按集合后端**——后端是服务端级 env 选择（见模块/策略文档）。
async fn create_collection(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<CreateCollectionBody>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    require_principal(&s, &headers)?;
    if body.name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must not be empty".into()));
    }
    let spec = CollectionSpec {
        dim: body.dim,
        distance: body.distance.clone(),
    };
    s.collections
        .lock()
        .await
        .insert(body.name.clone(), spec.clone());
    let info = server_vector_info(&s).await;
    Ok(Json(json!({
        "name": body.name,
        "dim": spec.dim,
        "distance": spec.distance.unwrap_or_else(|| "cosine".into()),
        "server": info,
    })))
}

/// 读回一个集合的咨询配置 + 服务端实际向量配置（introspection）。未注册返回 404。
async fn get_collection(
    State(s): State<ServerState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    require_principal(&s, &headers)?;
    let spec = s.collections.lock().await.get(&name).cloned();
    let Some(spec) = spec else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("collection '{name}' not registered"),
        ));
    };
    let info = server_vector_info(&s).await;
    Ok(Json(json!({
        "name": name,
        "dim": spec.dim,
        "distance": spec.distance.unwrap_or_else(|| "cosine".into()),
        "server": info,
    })))
}

/// 列出已注册集合名 + 服务端实际向量配置。
async fn list_collections(State(s): State<ServerState>, headers: HeaderMap) -> ApiResult {
    s.metrics.requests.fetch_add(1, Ordering::Relaxed);
    require_principal(&s, &headers)?;
    let mut names: Vec<String> = s.collections.lock().await.keys().cloned().collect();
    names.sort();
    let info = server_vector_info(&s).await;
    Ok(Json(json!({ "collections": names, "server": info })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use fastsearch_core::{AssetPointer, BBox, ChunkKind, MediaRef};
    use fastsearch_engine::ObjectStore;
    use fastsearch_text::TextIndexConfig;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct PairEmbedder;

    impl Embedder for PairEmbedder {
        fn dim(&self) -> usize {
            3
        }

        fn embed(&self, texts: &[String], _kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("red chart") {
                        vec![1.0, 0.0, 0.0]
                    } else {
                        vec![0.0, 1.0, 0.0]
                    }
                })
                .collect())
        }

        fn caps(&self) -> fastsearch_embed::EmbedCaps {
            fastsearch_embed::EmbedCaps {
                dim: 3,
                text: true,
                image: true,
                cross_modal: true,
                semantic: true,
            }
        }

        fn embed_multi(
            &self,
            inputs: &[EmbedInput],
            kind: EmbedKind,
        ) -> anyhow::Result<Vec<Vec<f32>>> {
            inputs
                .iter()
                .map(|i| match i {
                    EmbedInput::Text(t) => self
                        .embed(std::slice::from_ref(t), kind)
                        .map(|mut v| v.remove(0)),
                    EmbedInput::Image(bytes) if bytes == b"red-image" => Ok(vec![1.0, 0.0, 0.0]),
                    EmbedInput::Image(_) => Ok(vec![0.0, 1.0, 0.0]),
                })
                .collect()
        }
    }

    fn keys() -> HashMap<String, Principal> {
        let mut m = HashMap::new();
        m.insert(
            "k-team-a".into(),
            Principal {
                tenant: Some("acme".into()),
                tags: vec!["team-a".into()],
            },
        );
        m.insert(
            "k-team-b".into(),
            Principal {
                tenant: Some("acme".into()),
                tags: vec!["team-b".into()],
            },
        );
        m
    }

    #[test]
    fn ingest_acl_comes_from_principal_tags() {
        let tagged = Principal {
            tenant: Some("acme".into()),
            tags: vec!["team-a".into()],
        };
        assert_eq!(ingest_acl_for(&tagged), vec!["team-a".to_string()]);

        let untagged = Principal {
            tenant: None,
            tags: vec![],
        };
        assert_eq!(ingest_acl_for(&untagged), vec!["public".to_string()]);
    }

    #[test]
    fn image_status_initialization_does_not_claim_missing_bytes() {
        let mut c = chunk(1, "", vec!["team-a"]);
        c.kind = ChunkKind::Image;
        c.media = Some(MediaRef {
            asset: AssetPointer::Inline,
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        c.media_bytes = Some(b"image-bytes".to_vec());
        c.image_vector_status = Some(fastsearch_core::ImageVectorStatus::Pending);
        let ic = IndexChunk {
            chunk: c,
            vector: None,
        };
        assert_eq!(
            initial_image_vector_status(&ic),
            Some(fastsearch_core::ImageVectorStatus::Pending)
        );

        let with_vector = IndexChunk {
            vector: Some(vec![1.0, 0.0]),
            ..ic
        };
        assert_eq!(
            initial_image_vector_status(&with_vector),
            Some(fastsearch_core::ImageVectorStatus::Embedded)
        );
    }

    // ===================== MM6-signer：资产 URL 签名 + token 字节端点 =====================

    #[test]
    fn asset_signer_sign_verify_roundtrip() {
        let s = AssetSigner::new(b"secret".to_vec(), 300);
        let (exp, sig) = s.sign("kb:d.pdf:1", "image/png", 1000);
        assert_eq!(exp, 1300);
        assert!(s.verify("kb:d.pdf:1", exp, "image/png", &sig, 1000));
        assert!(
            s.verify("kb:d.pdf:1", exp, "image/png", &sig, 1299),
            "未过期"
        );
        assert!(
            !s.verify("kb:d.pdf:1", exp, "image/png", &sig, 1300),
            "过期(now>=exp)"
        );
        assert!(
            !s.verify("kb:d.pdf:2", exp, "image/png", &sig, 1000),
            "换 cid"
        );
        assert!(
            !s.verify("kb:d.pdf:1", exp, "image/jpeg", &sig, 1000),
            "换 ct"
        );
        assert!(
            !s.verify("kb:d.pdf:1", exp, "image/png", "deadbeef", 1000),
            "篡改 sig"
        );
        assert!(
            !AssetSigner::new(b"other".to_vec(), 300).verify(
                "kb:d.pdf:1",
                exp,
                "image/png",
                &sig,
                1000
            ),
            "换密钥"
        );
    }

    #[test]
    fn asset_signature_not_reusable_across_field_split() {
        // M20 回归：cid/ct 客户端可控、可含 `|`。旧 `cid|exp|ct` 拼接有规范化歧义——为 (cid1,ct1)
        // 签发的签名会对某个不同的 (cid2,exp2,ct2) 复用（拼出逐字节相等的消息）。长度前缀 framing 后
        // 不再可复用。
        let s = AssetSigner::new(b"secret-key".to_vec(), 500);
        let (cid1, ct1) = ("a", "b|100|c");
        let (exp1, sig1) = s.sign(cid1, ct1, 0);
        assert_eq!(exp1, 500);
        assert!(s.verify(cid1, exp1, ct1, &sig1, 0), "自身应验签通过");
        // 旧方案：msg1 = "a|500|b|100|c"；(cid2="a|500|b", exp2=100, ct2="c") 拼出同一 msg → 会复用。
        assert!(
            !s.verify("a|500|b", 100, "c", &sig1, 0),
            "跨 (cid,ct) 对复用同一签名应失败（长度前缀消歧）"
        );
    }

    /// 属性测试：`object_token_nonce()` 128 bit OS 随机 → 连续 mint 10k 次不重复。
    /// 守住"nonce 碰撞破坏 XOR 流密码机密性"不变量——如果将来有人把 nonce 改回时间+PID 构造，
    /// 多副本/重启场景下可能复现碰撞，这个测试会先挂。
    #[test]
    fn object_token_nonce_no_collision_under_rapid_mint() {
        use std::collections::HashSet;
        const N: usize = 10_000;
        let mut seen = HashSet::with_capacity(N);
        for _ in 0..N {
            let nonce = object_token_nonce();
            assert!(
                seen.insert(nonce),
                "nonce 重复：多副本/重启后 XOR 流密码会泄密"
            );
        }
    }

    /// 属性测试：`seal_object_target` 同 target 连签两次 → token 不同（nonce 每次重掷）；
    /// 但都能 `open_object_target` 还原出原 target。
    #[test]
    fn object_token_seal_is_non_deterministic_but_round_trips() {
        let s = AssetSigner::new(b"test-key".to_vec(), 300);
        let target = ObjectTokenTarget {
            cid: "kb:doc/with:colon.pdf:42".into(),
            uri: "s3://bucket/key.png".into(),
        };
        let t1 = s.seal_object_target(&target);
        let t2 = s.seal_object_target(&target);
        assert_ne!(t1, t2, "同 target 两次 seal 必须因 nonce 不同而 token 不同");

        let o1 = s.open_object_target(&t1).expect("t1 可还原");
        let o2 = s.open_object_target(&t2).expect("t2 可还原");
        assert_eq!(o1.cid, target.cid);
        assert_eq!(o1.uri, target.uri);
        assert_eq!(o2.cid, target.cid);
        assert_eq!(o2.uri, target.uri);
    }

    /// 边界：`open_object_target` 拒绝超大 token（防内存尖峰）。
    #[test]
    fn object_token_open_rejects_oversized_input() {
        let s = AssetSigner::new(b"test-key".to_vec(), 300);
        // 构造 >4KiB 的"token"：合法 base64 但超出 OBJECT_TOKEN_MAX_BYTES。
        let big = B64URL.encode(vec![1u8; 8 * 1024]);
        assert!(s.open_object_target(&big).is_none());
    }

    #[test]
    fn object_token_encoded_limit_rejects_before_decode() {
        let max = "A".repeat(OBJECT_TOKEN_MAX_ENCODED_BYTES);
        let over = "A".repeat(OBJECT_TOKEN_MAX_ENCODED_BYTES + 1);
        assert!(object_token_encoded_len_ok(&max));
        assert!(!object_token_encoded_len_ok(&over));
    }

    /// 边界：`ObjectTokenTarget` 拒绝未知字段（防攻击者塞噪声绕过解析）。
    #[test]
    fn object_token_target_rejects_unknown_fields() {
        let bad = r#"{"cid":"kb:d:1","uri":"s3://x","extra":"noise"}"#;
        let r: Result<ObjectTokenTarget, _> = serde_json::from_str(bad);
        assert!(
            r.is_err(),
            "带未知字段的 target 必须被 deny_unknown_fields 拒绝"
        );
    }

    fn signer_app() -> Router {
        // 无 source_pg：验签通过也取不到字节（隔离测 token 逻辑与字节面）。
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        router(ServerState::new(engine, keys()).with_asset_signer(b"k".to_vec(), 300))
    }

    fn bytes_uri(cid: &str, exp: u64, ct: &str, sig: &str) -> String {
        format!(
            "/v1/asset/{cid}/bytes?exp={exp}&ct={}&sig={sig}",
            ct.replace('/', "%2F")
        )
    }

    async fn get_status(app: Router, uri: String) -> StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    fn upload_image_request(doc_id: &str, text: &str, bytes: &[u8]) -> Request<Body> {
        let boundary = "fastsearch-upload-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"collection\"\r\n\r\nkb\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"doc_id\"\r\n\r\n{doc_id}\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\n{text}\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"img.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Request::builder()
            .method("POST")
            .uri("/v1/images")
            .header("x-api-key", "k-team-a")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn asset_bytes_valid_token_no_source_pg_is_404() {
        // 合法 token → 验签通过（非 403）；无 source_pg → 无字节 → 404（证明 token 已验、未误判 403）。
        let (exp, sig) =
            AssetSigner::new(b"k".to_vec(), 300).sign("kb:d.pdf:1", "image/png", unix_now());
        let st = get_status(
            signer_app(),
            bytes_uri("kb:d.pdf:1", exp, "image/png", &sig),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "验签过但无字节→404");
    }

    #[tokio::test]
    async fn asset_bytes_bad_sig_is_403() {
        let st = get_status(
            signer_app(),
            bytes_uri("kb:d.pdf:1", unix_now() + 300, "image/png", "deadbeef"),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "无效 sig→403");
    }

    #[tokio::test]
    async fn asset_bytes_expired_is_403() {
        // now=0 签 → exp=300；真实 now 远大于 300 → 过期。
        let (exp, sig) = AssetSigner::new(b"k".to_vec(), 300).sign("kb:d.pdf:1", "image/png", 0);
        let st = get_status(
            signer_app(),
            bytes_uri("kb:d.pdf:1", exp, "image/png", &sig),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "过期 token→403");
    }

    #[tokio::test]
    async fn asset_bytes_no_signer_is_403() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys())); // 未配签名器
        let st = get_status(
            app,
            bytes_uri("kb:d.pdf:1", 9_999_999_999, "image/png", "x"),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "未配签名器→403");
    }

    // --- inline 字节 Range 支持（serve_inline_bytes 纯函数；端到端取字节需 PG，env-gated）---

    fn hdr_range(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, v.parse().unwrap());
        h
    }

    async fn resp_parts(resp: Response) -> (StatusCode, HeaderMap, Vec<u8>) {
        let (p, body) = resp.into_parts();
        let b = body.collect().await.unwrap().to_bytes().to_vec();
        (p.status, p.headers, b)
    }

    #[tokio::test]
    async fn inline_bytes_no_range_is_200_with_accept_ranges() {
        let (st, h, body) = resp_parts(serve_inline_bytes(
            &HeaderMap::new(),
            "text/plain".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            h.get(header::ACCEPT_RANGES).unwrap(),
            "bytes",
            "应宣告支持 Range"
        );
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn inline_bytes_range_is_206_partial() {
        let (st, h, body) = resp_parts(serve_inline_bytes(
            &hdr_range("bytes=1-3"),
            "text/plain".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h.get(header::CONTENT_RANGE).unwrap(), "bytes 1-3/5");
        assert_eq!(h.get(header::ACCEPT_RANGES).unwrap(), "bytes");
        assert_eq!(body, b"ell", "闭区间含端");
    }

    #[tokio::test]
    async fn inline_bytes_suffix_and_open_ranges() {
        // 后缀 `-2` → 末 2 字节。
        let (_, h, body) = resp_parts(serve_inline_bytes(
            &hdr_range("bytes=-2"),
            "x".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(h.get(header::CONTENT_RANGE).unwrap(), "bytes 3-4/5");
        assert_eq!(body, b"lo");
        // 开区间 `2-` → 到末尾；末端越界自动截断到 total-1。
        let (_, h2, body2) = resp_parts(serve_inline_bytes(
            &hdr_range("bytes=2-99"),
            "x".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(h2.get(header::CONTENT_RANGE).unwrap(), "bytes 2-4/5");
        assert_eq!(body2, b"llo");
    }

    #[tokio::test]
    async fn inline_bytes_unsatisfiable_is_416() {
        let (st, h, _) = resp_parts(serve_inline_bytes(
            &hdr_range("bytes=10-20"),
            "x".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(st, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            h.get(header::CONTENT_RANGE).unwrap(),
            "bytes */5",
            "416 须带总长"
        );
    }

    #[tokio::test]
    async fn inline_bytes_multirange_falls_back_to_200() {
        let (st, _, body) = resp_parts(serve_inline_bytes(
            &hdr_range("bytes=0-1,3-4"),
            "x".into(),
            b"hello".to_vec(),
        ))
        .await;
        assert_eq!(st, StatusCode::OK, "多段 Range 不支持 → 退 200 全量");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn mint_inline_url_roundtrips_through_byte_endpoint() {
        // S3 签发 ↔ S2 验签闭环：mint 出的 URL 直接喂字节端点 → 验签通过（非 403）→ 无 source_pg → 404。
        let signer = AssetSigner::new(b"k".to_vec(), 300);
        let (url, expires_s) =
            mint_inline_url(&signer, "kb:d.pdf:1", Some("image/png"), unix_now());
        assert_eq!(expires_s, 300);
        assert!(url.starts_with("/v1/asset/") && url.contains("/bytes?"));
        assert!(url.contains("ct=image%2Fpng"), "ct 应百分号编码: {url}");
        let st = get_status(signer_app(), url).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "mint URL 验签过、无字节→404");
    }

    #[tokio::test]
    async fn assets_resolve_acl_not_bypassable() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()).with_asset_signer(b"k".to_vec(), 300));
        let body = r#"{"collection":"kb","doc_id":"rep.pdf","chunks":[
            {"doc_id":"rep.pdf","chunk_id":1,"kind":"image","text":"fig","page":7,
             "bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"char_len":3,"acl":["team-a"],"tenant":"acme",
             "media":{"asset":{"kind":"doc_region","page":7,"bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0}},
                      "media_type":"image/png"}}]}"#;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let resolve_req = |key: &str| {
            Request::builder()
                .method("POST")
                .uri("/v1/assets/resolve")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(r#"{"ids":["kb:rep.pdf:1"]}"#))
                .unwrap()
        };
        // 授权 team-a → assets 含 1 条 doc_render。
        let ok = app.clone().oneshot(resolve_req("k-team-a")).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let v = body_json(ok).await;
        assert_eq!(v["assets"].as_array().unwrap().len(), 1);
        assert_eq!(v["assets"][0]["type"], "doc_render");
        assert_eq!(v["assets"][0]["page"], 7);
        // 越权 team-b → assets 空（不暴露存在性，不可绕过）。
        let denied = app.oneshot(resolve_req("k-team-b")).await.unwrap();
        assert_eq!(denied.status(), StatusCode::OK);
        assert_eq!(
            body_json(denied).await["assets"].as_array().unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn object_asset_resolve_returns_token_url_and_serves_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let bytes = b"fake png bytes".to_vec();
        let obj = store.put("kb/img.png", &bytes, "image/png").unwrap();

        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let mut c = chunk(1, "object image", vec!["team-a"]);
        c.kind = ChunkKind::Image;
        c.media = Some(MediaRef {
            asset: AssetPointer::Object { uri: obj.uri },
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        engine.ingest("kb", &c).unwrap();
        engine.commit().unwrap();
        let state = ServerState::new(engine, keys())
            .with_asset_signer(b"k".to_vec(), 300)
            .enable_object_url_signer()
            .await;
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/assets/resolve")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"ids":["kb:rep.pdf:1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let url = v["assets"][0]["url"].as_str().unwrap();
        assert!(url.starts_with("/v1/object/"));
        assert!(!url.contains("s3://"));
        assert!(!url.contains("kb/img.png"));

        let resp = app
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn object_token_url_is_not_bound_to_issuing_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let bytes = b"replica readable object".to_vec();
        let obj = store.put("kb/img.png", &bytes, "image/png").unwrap();

        let make_engine = || {
            let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
            engine.set_object_store(store.clone());
            let mut c = chunk(1, "object image", vec!["team-a"]);
            c.kind = ChunkKind::Image;
            c.media = Some(MediaRef {
                asset: AssetPointer::Object {
                    uri: obj.uri.clone(),
                },
                media_type: Some("image/png".into()),
                time: None,
                region: None,
                caption_source: None,
                thumbnail: None,
            });
            engine.ingest("kb", &c).unwrap();
            engine.commit().unwrap();
            engine
        };

        let issuer = router(
            ServerState::new(make_engine(), keys())
                .with_asset_signer(b"k".to_vec(), 300)
                .enable_object_url_signer()
                .await,
        );
        let resp = issuer
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/assets/resolve")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"ids":["kb:rep.pdf:1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let url = body_json(resp).await["assets"][0]["url"]
            .as_str()
            .unwrap()
            .to_string();

        let reader =
            router(ServerState::new(make_engine(), keys()).with_asset_signer(b"k".to_vec(), 300));
        let resp = reader
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn index_validates_object_refs_for_default_store_media() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let app = router(ServerState::new(engine, keys()));
        let body = r#"{"collection":"kb","doc_id":"rep.pdf","chunks":[
            {"doc_id":"rep.pdf","chunk_id":1,"kind":"image","text":"missing","page":1,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":7,"acl":["team-a"],"tenant":"acme",
             "media":{"asset":{"kind":"object","uri":"s3://assets/missing.png"},"media_type":"image/png"}}]}"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn index_rejects_cross_tenant_object_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let obj = store
            .put("other/img.png", b"tenant b bytes", "image/png")
            .unwrap();
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let app = router(ServerState::new(engine, keys()));
        let body = serde_json::to_string(&json!({
            "collection": "kb",
            "doc_id": "rep.pdf",
            "store_media": "reference",
            "chunks": [{
                "doc_id": "rep.pdf",
                "chunk_id": 1,
                "kind": "image",
                "text": "cross tenant",
                "page": 1,
                "bbox": {"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},
                "char_len": 12,
                "acl": ["team-a"],
                "tenant": "acme",
                "media": {"asset": {"kind": "object", "uri": obj.uri}, "media_type": "image/png"}
            }]
        }))
        .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn object_bytes_missing_object_is_404_not_500() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let mut c = chunk(1, "missing object", vec!["team-a"]);
        c.kind = ChunkKind::Image;
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://assets/acme/missing.png".into(),
            },
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        engine.ingest("kb", &c).unwrap();
        engine.commit().unwrap();
        let state = ServerState::new(engine, keys()).with_asset_signer(b"k".to_vec(), 300);
        let signer = AssetSigner::new(b"k".to_vec(), 300);
        let (url, _) = mint_object_url(
            &signer,
            "kb:rep.pdf:1",
            "s3://assets/acme/missing.png",
            Some("image/png"),
            unix_now(),
        );
        let app = router(state);
        let resp = app
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn replacing_object_doc_cleans_old_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let state = ServerState::new(engine, keys())
            .with_asset_signer(b"k".to_vec(), 300)
            .enable_object_url_signer()
            .await;
        let app = router(state);

        let first = app
            .clone()
            .oneshot(upload_image_request(
                "img-1",
                "first caption",
                b"first-image",
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let resolved = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/assets/resolve")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"ids":["kb:img-1:1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let old_url = body_json(resolved).await["assets"][0]["url"]
            .as_str()
            .unwrap()
            .to_string();

        let second = app
            .clone()
            .oneshot(upload_image_request(
                "img-1",
                "second caption",
                b"second-image",
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);

        let old = app
            .oneshot(
                Request::builder()
                    .uri(&old_url)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::NOT_FOUND);
    }

    fn chunk(id: u64, text: &str, acl: Vec<&str>) -> Chunk {
        Chunk {
            doc_id: "rep.pdf".into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: text.into(),
            page: id as u32,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 0,
            char_len: text.len() as u32,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: Some("acme".into()),
            acl: acl.into_iter().map(String::from).collect(),
        }
    }

    async fn app_with_data() -> Router {
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine
            .ingest("kb", &chunk(1, "secret alpha", vec!["team-a"]))
            .unwrap();
        engine
            .ingest("kb", &chunk(2, "secret beta", vec!["team-b"]))
            .unwrap();
        engine.commit().unwrap();
        router(ServerState::new(engine, keys()))
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_no_auth() {
        let app = app_with_data().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn search_requires_auth() {
        let app = app_with_data().await;
        // 无 key
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // 错 key
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "bogus")
                    .body(Body::from(r#"{"query":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn acl_not_bypassable() {
        let app = app_with_data().await;
        // team-a 的 key 搜 "secret" → 只能看到 chunk 1（team-a），看不到 chunk 2（team-b）
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer k-team-a")
                    .body(Body::from(r#"{"query":"secret","top_k":10}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["chunk_id"], 1);
    }

    #[tokio::test]
    async fn index_then_search() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        // index
        let body = r#"{"collection":"kb","doc_id":"d.pdf","chunks":[
            {"doc_id":"d.pdf","chunk_id":1,"kind":"paragraph","text":"hello world","page":7,
             "bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"char_len":11,"acl":["team-a"],"tenant":"acme"}]}"#;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["indexed"], 1);
        // search
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"query":"hello","top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["page"], 7);
        assert_eq!(hits[0]["citation_id"], "kb:d.pdf:1");
    }

    #[tokio::test]
    async fn index_acl_and_tenant_are_injected_from_principal() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let body = r#"{"collection":"kb","doc_id":"d.pdf","chunks":[
            {"doc_id":"client-controlled.pdf","chunk_id":1,"kind":"paragraph","text":"server injected identity","page":1,
             "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":24,"acl":["team-b"],"tenant":"evil"}]}"#;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = r#"{"query":"server injected identity","top_k":5}"#;
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-b")
                    .body(Body::from(req))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(body_json(denied).await["hits"].as_array().unwrap().len(), 0);

        let allowed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(req))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(allowed).await;
        assert_eq!(v["hits"].as_array().unwrap().len(), 1);
        assert_eq!(v["hits"][0]["citation_id"], "kb:d.pdf:1");
    }

    #[tokio::test]
    async fn delete_doc_accepts_slash_in_doc_id() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let body = r#"{"collection":"kb","doc_id":"sub/d.md","chunks":[
            {"doc_id":"sub/d.md","chunk_id":1,"kind":"paragraph","text":"slash doc","page":1,
             "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},"char_len":9,"acl":["team-a"],"tenant":"acme"}]}"#;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/docs/kb/sub/d.md")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["deleted"], true);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"query":"slash","top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(body_json(resp).await["hits"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_missing_doc_is_404() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/docs/kb/missing")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn text_query_retrieves_image_via_cross_modal_vector() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()).with_embedder(Arc::new(PairEmbedder)));
        let body = serde_json::to_string(&json!({
            "collection": "kb",
            "doc_id": "image-doc",
            "chunks": [{
                "doc_id": "image-doc",
                "chunk_id": 1,
                "kind": "image",
                "text": "",
                "page": 3,
                "bbox": {"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},
                "char_len": 0,
                "acl": ["team-a"],
                "tenant": "acme",
                "media": {"asset": {"kind": "inline"}, "media_type": "image/png"},
                "media_bytes": b"red-image".to_vec()
            }]
        }))
        .unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = SearchRequest {
            query: "red chart".into(),
            mode: SearchMode::Hybrid,
            top_k: 5,
            candidates: 20,
            filter: Some(Filter::And(vec![
                Filter::Eq("collection".into(), FieldValue::Str("kb".into())),
                Filter::Eq("modality".into(), FieldValue::Str("image".into())),
            ])),
            ..Default::default()
        };
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(serde_json::to_string(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["citation_id"], "kb:image-doc:1");
        assert_eq!(hits[0]["page"], 3);
    }

    #[tokio::test]
    async fn image_upload_resolve_delete_cleans_index_and_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(fastsearch_engine::LocalObjectStore::new(
            tmp.path(),
            "assets",
        ));
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_object_store(store);
        let state = ServerState::new(engine, keys())
            .with_asset_signer(b"k".to_vec(), 300)
            .enable_object_url_signer()
            .await;
        let app = router(state);

        let boundary = "fastsearch-upload-boundary";
        let img = b"uploaded-image-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"collection\"\r\n\r\nkb\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"doc_id\"\r\n\r\nimg-1\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\nupload caption\r\n").as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"img.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(img);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images")
                    .header("x-api-key", "k-team-a")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["indexed"], 1);

        let req = SearchRequest {
            query: "upload caption".into(),
            top_k: 5,
            filter: Some(Filter::And(vec![
                Filter::Eq("collection".into(), FieldValue::Str("kb".into())),
                Filter::Eq("modality".into(), FieldValue::Str("image".into())),
            ])),
            ..Default::default()
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(serde_json::to_string(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let hits = body_json(resp).await["hits"].as_array().unwrap().clone();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["citation_id"], "kb:img-1:1");

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-b")
                    .body(Body::from(serde_json::to_string(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["hits"].as_array().unwrap().len(), 0);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/assets/resolve")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-b")
                    .body(Body::from(r#"{"ids":["kb:img-1:1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["assets"].as_array().unwrap().len(), 0);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/assets/resolve")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"ids":["kb:img-1:1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resolved = body_json(resp).await;
        let url = resolved["assets"][0]["url"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            img
        );

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/docs/kb/img-1")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let deleted = body_json(resp).await;
        assert_eq!(deleted["deleted"], true);
        assert_eq!(deleted["objects_deleted"], 1);

        let req = SearchRequest {
            query: "upload caption".into(),
            top_k: 5,
            filter: Some(Filter::Eq(
                "collection".into(),
                FieldValue::Str("kb".into()),
            )),
            ..Default::default()
        };
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(serde_json::to_string(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(body_json(resp).await["hits"].as_array().unwrap().len(), 0);

        let resp = app
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // 预计算向量旁路：chunk 携带 `vector` 时跳过嵌入直接入向量索引，纯向量检索可命中。
    // 无嵌入后端（ServerState::new 不配 embedder）也能用——benchmark 正是这条路径。
    #[tokio::test]
    async fn index_with_precomputed_vector_then_vector_search() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let body = r#"{"collection":"kb","doc_id":"d","chunks":[
            {"doc_id":"d","chunk_id":1,"kind":"paragraph","text":"","page":0,
             "bbox":{"x0":0.0,"y0":0.0,"x1":0.0,"y1":0.0},"char_len":0,"acl":["team-a"],"tenant":"acme",
             "vector":[1.0,0.0,0.0]},
            {"doc_id":"d","chunk_id":2,"kind":"paragraph","text":"","page":0,
             "bbox":{"x0":0.0,"y0":0.0,"x1":0.0,"y1":0.0},"char_len":0,"acl":["team-a"],"tenant":"acme",
             "vector":[0.0,1.0,0.0]}]}"#;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["indexed"], 2);
        // 纯向量检索：查询向量贴近 chunk_id=1，应排第一。
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(
                        r#"{"query":"","mode":"vector","vector":[1.0,0.0,0.0],"top_k":5}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "vector search should return hits");
        assert_eq!(hits[0]["chunk_id"], 1);
    }

    // 集合注册 + introspection：POST 回显配置且带服务端实际后端；GET 读回；列表含名。
    #[tokio::test]
    async fn collections_register_and_introspect() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let post = |body: &'static str| {
            Request::builder()
                .method("POST")
                .uri("/v1/collections")
                .header("content-type", "application/json")
                .header("x-api-key", "k-team-a")
                .body(Body::from(body))
                .unwrap()
        };
        let resp = app
            .clone()
            .oneshot(post(r#"{"name":"kb","dim":3}"#))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["name"], "kb");
        assert_eq!(v["dim"], 3);
        assert_eq!(v["distance"], "cosine"); // 默认
        assert_eq!(v["server"]["vector_backend"], "brute"); // 服务端实际后端
        assert_eq!(v["server"]["embedded"], false);

        // GET 读回
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/collections/kb")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["dim"], 3);

        // 未注册 → 404
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/collections/nope")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // 列表
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/collections")
                    .header("x-api-key", "k-team-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["collections"], json!(["kb"]));
    }

    // 注册 dim 后，ingest 携带不等维预计算向量 → 400（benchmark 误配早失败）。
    #[tokio::test]
    async fn index_dim_mismatch_rejected() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/collections")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"name":"kb","dim":4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // dim=4 注册，传 3 维向量 → 400
        let body = r#"{"collection":"kb","doc_id":"d","chunks":[
            {"doc_id":"d","chunk_id":1,"kind":"paragraph","text":"","page":0,
             "bbox":{"x0":0.0,"y0":0.0,"x1":0.0,"y1":0.0},"char_len":0,"acl":["team-a"],"tenant":"acme",
             "vector":[1.0,0.0,0.0]}]}"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // 无鉴权 → 401。
    #[tokio::test]
    async fn collections_require_auth() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/collections")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"kb"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bad_body_400() {
        let app = app_with_data().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from("{not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn metrics_expose_counters_and_histogram() {
        let app = app_with_data().await;
        // 一次成功检索
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer k-team-a")
                    .body(Body::from(r#"{"query":"secret","top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // 一次未授权
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("fastsearch_searches_total 1"));
        assert!(text.contains("fastsearch_unauthorized_total 1"));
        assert!(text.contains("fastsearch_search_latency_seconds_count 1"));
        assert!(text.contains("fastsearch_search_latency_seconds_bucket{le=\"+Inf\"} 1"));
        assert!(text.contains("# TYPE fastsearch_search_latency_seconds histogram"));
    }

    async fn engine_with_data() -> Engine {
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine
            .ingest("kb", &chunk(1, "secret alpha", vec!["team-a"]))
            .unwrap();
        engine.commit().unwrap();
        engine
    }

    fn search_req(key: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/search")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(r#"{"query":"secret","top_k":5}"#))
            .unwrap()
    }

    #[tokio::test]
    async fn rate_limit_returns_429() {
        // 容量 1、无回填 → 同 key 第二次必 429。
        let state = ServerState::new(engine_with_data().await, keys()).with_rate_limit(1.0, 0.0);
        let app = router(state);
        let r1 = app.clone().oneshot(search_req("k-team-a")).await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let r2 = app.clone().oneshot(search_req("k-team-a")).await.unwrap();
        assert_eq!(r2.status(), StatusCode::TOO_MANY_REQUESTS);
        // 指标计数
        let m = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let text =
            String::from_utf8(m.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(text.contains("fastsearch_rate_limited_total 1"));
    }

    #[tokio::test]
    async fn audit_sink_receives_event() {
        let captured: Arc<std::sync::Mutex<Vec<AuditEvent>>> =
            Arc::new(std::sync::Mutex::new(vec![]));
        let cap2 = captured.clone();
        let sink: AuditSink = Arc::new(move |ev| cap2.lock().unwrap().push(ev));
        let state = ServerState::new(engine_with_data().await, keys()).with_audit(sink);
        let app = router(state);
        app.oneshot(search_req("k-team-a")).await.unwrap();
        let evs = captured.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].endpoint, "/v1/search");
        assert_eq!(evs[0].query.as_deref(), Some("secret"));
        assert_eq!(evs[0].tenant.as_deref(), Some("acme"));
        assert_eq!(evs[0].hits, Some(1));
        assert_eq!(evs[0].status, 200);
    }

    #[tokio::test]
    async fn similar_endpoint_excludes_seed() {
        // 灌入两条共享词 + 一条无关，对种子求相似。
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        for (id, txt) in [
            (1, "alpha beta gamma"),
            (2, "beta gamma delta"),
            (3, "zzz qqq"),
        ] {
            engine
                .ingest("kb", &chunk(id, txt, vec!["team-a"]))
                .unwrap();
        }
        engine.commit().unwrap();
        let app = router(ServerState::new(engine, keys()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/similar")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(r#"{"citation_id":"kb:rep.pdf:1","top_k":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        // 不含种子 chunk 1；含相似 chunk 2
        assert!(hits.iter().all(|h| h["chunk_id"] != 1));
        assert!(hits.iter().any(|h| h["chunk_id"] == 2));
    }

    #[tokio::test]
    async fn openapi_served_no_auth() {
        let app = app_with_data().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["openapi"], "3.0.3");
        assert!(v["paths"]["/v1/search"]["post"].is_object());
        assert!(v["paths"]["/v1/index"]["post"].is_object());
        // MM6-signer 两端点入契约（四张脸契约一致）。
        assert!(v["paths"]["/v1/assets/resolve"]["post"].is_object());
        assert!(v["paths"]["/v1/asset/{citation_id}/bytes"]["get"].is_object());
        assert!(v["components"]["schemas"]["SearchRequest"].is_object());
        assert!(
            v["components"]["schemas"]["SearchRequest"]["properties"]["query_image_base64"]
                .is_object()
        );
        assert!(
            v["paths"]["/v1/search"]["post"]["requestBody"]["content"]["multipart/form-data"]
                .is_object()
        );
        // 版本来自 crate 版本，非空
        assert!(v["info"]["version"].as_str().unwrap().len() >= 3);
    }

    #[test]
    fn search_json_query_image_base64_decodes_and_blocks_internal_field() {
        let req = decode_search_value(json!({
            "query": "",
            "mode": "vector",
            "query_image_base64": "AQIDBA=="
        }))
        .unwrap();
        assert_eq!(req.query_image, Some(vec![1, 2, 3, 4]));

        let err = decode_search_value(json!({"query": "", "query_image": [1, 2]}))
            .unwrap_err()
            .1;
        assert!(err.contains("query_image is internal"), "got: {err}");
    }

    #[tokio::test]
    async fn multipart_image_search_accepts_payload_above_axum_default_limit() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let boundary = "fastsearch-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{}\r\n",
                r#"{"query":"","mode":"keyword","top_k":1}"#
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"q.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend(std::iter::repeat_n(0x42, 3 * 1024 * 1024));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("x-api-key", "k-team-a")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn hits_json_redacts_object_media() {
        use fastsearch_core::{AssetPointer, BBox, Citation, GlobalId, MediaRef};
        let hit = fastsearch_engine::SearchHit {
            id: GlobalId {
                collection: "kb".into(),
                doc_id: "img.png".into(),
                chunk_id: 1,
            },
            score: 1.0,
            citation: Citation {
                collection: "kb".into(),
                doc_id: "img.png".into(),
                chunk_id: 1,
                page: 1,
                bbox: BBox {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0,
                },
                heading_path: vec![],
                section_id: 0,
                time: None,
                media: Some(MediaRef {
                    asset: AssetPointer::Object {
                        uri: "s3://private-bucket/secret/key.png".into(),
                    },
                    media_type: Some("image/png".into()),
                    time: None,
                    region: None,
                    caption_source: None,
                    thumbnail: None,
                }),
            },
            bm25: None,
            vector: Some(1.0),
            rerank: None,
            highlight: None,
            merged_chunk_ids: vec![],
        };
        let signer = AssetSigner::new(b"k".to_vec(), 300);
        let s = serde_json::to_string(&hits_json(
            &[hit],
            Some(&signer),
            Some("https://fastsearch.example"),
        ))
        .unwrap();
        assert!(s.contains(r#""kind":"object""#));
        assert!(s.contains(r#""url":"https://fastsearch.example/v1/object/"#));
        assert!(!s.contains("s3://"));
        assert!(!s.contains("private-bucket"));
        assert!(!s.contains("secret/key.png"));
    }

    /// 真语义混合（env-gated，需本地 Ollama）：经 server 灌入带嵌入的 passage，再用
    /// **语义相关但词面不重叠**的查询走 vector 模式，断言语义最近的 chunk 居首。
    /// 例：`FASTSEARCH_EMBED_TEST_URL=http://localhost:11434 FASTSEARCH_EMBED_MODEL=nomic-embed-text-v2-moe \
    ///      FASTSEARCH_EMBED_DIM=768 cargo test -p fastsearch-server semantic_hybrid -- --nocapture`
    #[tokio::test]
    async fn semantic_hybrid_via_server_gated() {
        let Ok(url) = std::env::var("FASTSEARCH_EMBED_TEST_URL") else {
            eprintln!("skip semantic_hybrid_via_server_gated: FASTSEARCH_EMBED_TEST_URL not set");
            return;
        };
        let mut ecfg = fastsearch_embed::EmbedderConfig::from_env();
        ecfg.url = url;
        if !matches!(ecfg.kind, fastsearch_embed::EmbedderKind::Http(_)) {
            ecfg.kind =
                fastsearch_embed::EmbedderKind::Http(fastsearch_embed::HttpProtocol::Ollama);
        }
        let embedder = Arc::from(fastsearch_embed::build_embedder(&ecfg));
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let state = ServerState::new(engine, keys()).with_embedder(embedder);
        let app = router(state);

        // 灌入两段：A=盈利能力，B=停车安排（公开 acl）。
        let body = r#"{"collection":"kb","doc_id":"rep.pdf","chunks":[
            {"doc_id":"rep.pdf","chunk_id":1,"kind":"paragraph","text":"本季度公司盈利能力显著改善，净利润增长。","page":1,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":18,"acl":["team-a"],"tenant":"acme"},
            {"doc_id":"rep.pdf","chunk_id":2,"kind":"paragraph","text":"新办公楼的访客停车位安排与门禁说明。","page":2,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":17,"acl":["team-a"],"tenant":"acme"}]}"#;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // 语义查询（与 A 词面几乎不重叠）走 vector 模式 → A 应居首。
        let q = r#"{"query":"企业的赚钱能力如何","mode":"vector","top_k":5}"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(q))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let hits = v["hits"].as_array().unwrap();
        assert!(!hits.is_empty(), "vector search returned no hits");
        assert_eq!(
            hits[0]["chunk_id"], 1,
            "semantically closest chunk should rank first"
        );
        assert!(
            hits[0]["vector"].as_f64().is_some(),
            "vector score should be present"
        );
    }

    #[tokio::test]
    async fn search_after_paginates_over_rest() {
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        // 灌 3 条命中同词的 chunk。
        let body = r#"{"collection":"kb","doc_id":"d.pdf","chunks":[
            {"doc_id":"d.pdf","chunk_id":1,"kind":"paragraph","text":"data alpha","page":1,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":10,"acl":["team-a"],"tenant":"acme"},
            {"doc_id":"d.pdf","chunk_id":2,"kind":"paragraph","text":"data beta","page":1,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":9,"acl":["team-a"],"tenant":"acme"},
            {"doc_id":"d.pdf","chunk_id":3,"kind":"paragraph","text":"data gamma","page":1,
             "bbox":{"x0":0,"y0":0,"x1":1,"y1":1},"char_len":10,"acl":["team-a"],"tenant":"acme"}]}"#;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let search = |body: String| {
            app.clone().oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
        };
        // 第一页 top_k=2
        let p1 = body_json(
            search(r#"{"query":"data","mode":"keyword","top_k":2}"#.into())
                .await
                .unwrap(),
        )
        .await;
        let h1 = p1["hits"].as_array().unwrap();
        assert_eq!(h1.len(), 2);
        let cursor = h1[1]["cursor"].as_str().unwrap().to_string();

        // 第二页：search_after=上一页末条 cursor → 接续，不与第一页重叠。
        let p2 = body_json(
            search(format!(
                r#"{{"query":"data","mode":"keyword","top_k":2,"search_after":"{cursor}"}}"#
            ))
            .await
            .unwrap(),
        )
        .await;
        let h2 = p2["hits"].as_array().unwrap();
        assert_eq!(h2.len(), 1); // 共 3 条，第二页剩 1 条
        let page1_ids: Vec<&str> = h1
            .iter()
            .map(|h| h["citation_id"].as_str().unwrap())
            .collect();
        let p2_id = h2[0]["citation_id"].as_str().unwrap();
        assert!(!page1_ids.contains(&p2_id), "第二页不应与第一页重叠");
    }

    #[tokio::test]
    async fn asset_acl_not_bypassable() {
        // 灌入带 media(DocRegion) 的图 chunk，acl=team-a。
        let engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        let app = router(ServerState::new(engine, keys()));
        let body = r#"{"collection":"kb","doc_id":"rep.pdf","chunks":[
            {"doc_id":"rep.pdf","chunk_id":1,"kind":"image","text":"figure caption","page":7,
             "bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0},"char_len":14,"acl":["team-a"],"tenant":"acme",
             "media":{"asset":{"kind":"doc_region","page":7,"bbox":{"x0":1.0,"y0":2.0,"x1":3.0,"y1":4.0}},
                      "media_type":"image/png"}}]}"#;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/index")
                    .header("content-type", "application/json")
                    .header("x-api-key", "k-team-a")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let asset_req = |key: &str| {
            Request::builder()
                .uri("/v1/asset/kb:rep.pdf:1")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap()
        };
        // 授权 team-a → 200 doc_render
        let ok = app.clone().oneshot(asset_req("k-team-a")).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let v = body_json(ok).await;
        assert_eq!(v["type"], "doc_render");
        assert_eq!(v["page"], 7);
        // 越权 team-b → 404（不暴露存在性，不可绕过）
        let denied = app.clone().oneshot(asset_req("k-team-b")).await.unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        // 无 key → 401
        let noauth = app
            .oneshot(
                Request::builder()
                    .uri("/v1/asset/kb:rep.pdf:1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(noauth.status(), StatusCode::UNAUTHORIZED);
    }

    /// MM6-inline 服务端 E2E（需 DATABASE_URL + multi-thread）：`GET /v1/asset/{cid}` 经
    /// auth→handler→engine→source_pg 吐 PG `media_bytes` 真源字节；越权 404、无 key 401。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn asset_inline_bytes_e2e() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip asset_inline_bytes_e2e: DATABASE_URL not set");
            return;
        };
        use fastsearch_core::{AssetPointer, BBox, Chunk, ChunkKind, MediaRef};
        use fastsearch_pg::{PgConfig, PgStore};

        // 带 inline 字节的图 chunk（无 caption），限 team-a 可见。
        let bytes = vec![0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A];
        let mut c = Chunk {
            doc_id: "img.pdf".into(),
            chunk_id: 1,
            kind: ChunkKind::Image,
            text: String::new(),
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 0,
            char_len: 0,
            media: Some(MediaRef {
                asset: AssetPointer::Inline,
                media_type: Some("image/png".into()),
                time: None,
                region: None,
                caption_source: None,
                thumbnail: None,
            }),
            media_bytes: None,
            image_vector_status: None,
            tenant: Some("acme".into()),
            acl: vec!["team-a".into()],
        };

        // 引擎索引：放 chunk（resolve 取 MediaRef）。PG 真源：放字节。
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.ingest("kb", &c).unwrap();
        engine.commit().unwrap();

        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_srv_mb_it".into();
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");
        c.media_bytes = Some(bytes.clone());
        store
            .upsert_doc("kb", "img.pdf", &[c])
            .await
            .expect("upsert");
        engine.set_source_store(std::sync::Arc::new(store));

        let app = router(ServerState::new(engine, keys()));
        let asset_req = |key: &str| {
            Request::builder()
                .uri("/v1/asset/kb:img.pdf:1")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap()
        };
        // 授权 → 200 + image/png + 真源字节。
        let ok = app.clone().oneshot(asset_req("k-team-a")).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers().get("content-type").unwrap(), "image/png");
        let got = axum::body::to_bytes(ok.into_body(), 1 << 20).await.unwrap();
        assert_eq!(got.as_ref(), bytes.as_slice(), "应吐 PG 真源字节");
        // 越权 → 404（不暴露存在性/字节）。
        let denied = app.clone().oneshot(asset_req("k-team-b")).await.unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        // 无 key → 401。
        let noauth = app
            .oneshot(
                Request::builder()
                    .uri("/v1/asset/kb:img.pdf:1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(noauth.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn pure_auth_and_acl() {
        let ks = keys();
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "k-team-a".parse().unwrap());
        let p = principal_from_headers(&h, &ks).unwrap();
        assert_eq!(p.tenant.as_deref(), Some("acme"));
        let acl = acl_for(&p);
        assert_eq!(acl.allowed_tags, vec!["team-a".to_string()]);
        // 无 header → None
        assert!(principal_from_headers(&HeaderMap::new(), &ks).is_none());
    }
}
