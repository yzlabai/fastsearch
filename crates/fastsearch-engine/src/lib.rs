//! # fastsearch-engine
//!
//! 把 core（融合/模型）+ text（全文索引）+ vector（向量后端）+ sync（CDC sink）
//! 整合成端到端引擎：灌入（含经 CDC `IndexSink`）→ 索引 → 排序管线检索 → 带引用命中。
//! 详见 [spec](../../docs/specs/14-engine.md)。
//!
//! 三种检索模式全可用：keyword / vector / **hybrid（keyword∥vector → core::fuse 融合）**。
//! 过滤与 ACL 在两路各自做真预过滤（不可绕过）；分面（kind/doc_id）、高亮、**rerank**
//! （req.rerank 时宽召回后重排）、**auto-merging**（req.auto_merge 同 section 归并）、
//! **分组折叠**（req.collapse 每 doc/section 限 N 条）均已接入。

use fastsearch_core::{
    fuse, AclFilter, AssetPointer, BBox, Chunk, ChunkKind, Citation, FieldValue, Filter, GlobalId,
    Scored, SearchMode, SearchRequest, TimeSpan,
};
use fastsearch_embed::{EmbedInput, EmbedKind, Embedder};
use fastsearch_rerank::{LexicalOverlapReranker, Reranker};
use fastsearch_sync::replication::{advance_slot, peek_with_lsn, ReplicationConfig};
use fastsearch_sync::{Applier, Lsn};
use fastsearch_text::{TextHit, TextIndex, TextIndexConfig};
pub use fastsearch_vector::{HnswParams, VectorBackendKind, DEFAULT_BINARY_OVERSAMPLE};
use fastsearch_vector::{VecMeta, VectorBackend, VectorStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("text index error: {0}")]
    Text(#[from] fastsearch_text::TextError),
    #[error("core error: {0}")]
    Core(#[from] fastsearch_core::CoreError),
    #[error("vector error: {0}")]
    Vector(String),
    #[error("rerank error: {0}")]
    Rerank(String),
    #[error("persist error: {0}")]
    Persist(String),
    #[error("cdc error: {0}")]
    Cdc(String),
}
pub type Result<T> = std::result::Result<T, EngineError>;

fn vector_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("vector.bin")
}

/// pgvector 直查档的过取系数（PG 取 `candidates × 此值` 候选再精确后过滤，抵消损耗）。
const PG_VECTOR_OVER_FETCH: usize = 4;

/// CDC 检查点：派生索引落盘时一并记录的水位（崩溃后从此续传）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
    schema_version: u32,
    /// 已持久化进派生索引的 LSN 水位。
    applied_lsn: u64,
    /// 向量维度（用于检测换模型/换维度需重建）。
    vector_dim: Option<usize>,
    /// 向量后端名（"brute"/"hnsw"）；open 时据此选 loader。空/缺省视为 brute。
    #[serde(default)]
    vector_backend: String,
}

impl Checkpoint {
    fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("checkpoint.json")
    }

    fn load(data_dir: &Path) -> Result<Self> {
        let p = Self::path(data_dir);
        if !p.exists() {
            return Ok(Self::default());
        }
        let bytes =
            std::fs::read(&p).map_err(|e| EngineError::Persist(format!("read checkpoint: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| EngineError::Persist(format!("parse checkpoint: {e}")))
    }

    /// 原子写：tmp → fsync → rename。
    fn save(&self, data_dir: &Path) -> Result<()> {
        let p = Self::path(data_dir);
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let bytes = serde_json::to_vec(self)
            .map_err(|e| EngineError::Persist(format!("ser checkpoint: {e}")))?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| EngineError::Persist(format!("create checkpoint tmp: {e}")))?;
            f.write_all(&bytes)
                .and_then(|_| f.sync_all())
                .map_err(|e| EngineError::Persist(format!("write checkpoint: {e}")))?;
        }
        std::fs::rename(&tmp, &p)
            .map_err(|e| EngineError::Persist(format!("rename checkpoint: {e}")))?;
        Ok(())
    }
}

fn kind_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Heading => "heading",
        ChunkKind::Paragraph => "paragraph",
        ChunkKind::Table => "table",
        ChunkKind::Code => "code",
        ChunkKind::ListItem => "list_item",
        ChunkKind::Image => "image",
        ChunkKind::Audio => "audio",
        ChunkKind::Video => "video",
    }
}

/// 同步↔异步桥：在 CDC 落地（同步 `IndexSink`）里调 PG 异步写穿。**要求 multi-thread tokio
/// runtime**（与 `set_pg_vector`/B6 检索同约束）；仅在配了 `vector_pg` 时触达，故约束一致。
fn block_on_pg<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

fn vec_meta(collection: &str, c: &Chunk) -> VecMeta {
    VecMeta {
        collection: collection.to_string(),
        doc_id: c.doc_id.clone(),
        chunk_id: c.chunk_id,
        kind: kind_str(c.kind).to_string(),
        modality: c.kind.modality().as_str().to_string(),
        page: c.page,
        section_id: c.section_id,
        heading_path: c.heading_path.clone(),
        tenant: c.tenant.clone(),
        acl: c.acl.clone(),
        bbox: c.bbox,
        time: c.media.as_ref().and_then(|m| m.time),
        media: c.media.clone(),
    }
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

/// 一条检索命中（带引用与分数明细）。
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: GlobalId,
    pub score: f64,
    pub citation: Citation,
    /// BM25 分（keyword/hybrid 路）。
    pub bm25: Option<f32>,
    /// 向量相似分（hybrid 路；vector 后端落地前为 None）。
    pub vector: Option<f64>,
    /// rerank 分（req.rerank 存在时）。
    pub rerank: Option<f64>,
    /// 高亮片段（HTML）；仅 keyword 命中且 req.highlight 时有值。
    pub highlight: Option<String>,
    /// auto-merge 时被并入本代表命中的同 section 兄弟 chunk_id（升序）；未归并为空。
    /// 答案层可据此解析整段的全部引用。
    pub merged_chunk_ids: Vec<u64>,
}

impl SearchHit {
    /// 最终排名的排序键：有 rerank 用 rerank 分，否则用融合分（与 `run` 末端排序一致）。
    fn sort_key(&self) -> f64 {
        self.rerank.unwrap_or(self.score)
    }

    /// 本命中的**深分页游标**（不透明 token）：把它作为下一页的 `search_after` 即从此条之后续取。
    /// 编码 = `排序键 bits(16 hex)` + `:` + `citation_id`（精确 round-trip，确定性）。
    pub fn cursor(&self) -> String {
        format!(
            "{:016x}:{}",
            self.sort_key().to_bits(),
            self.id.to_citation_id()
        )
    }
}

/// 解析深分页游标 → `(排序键, GlobalId)`。前 16 位十六进制是排序键 bits，其后为 citation_id
/// （doc_id 可含 `:`，故只在第 17 位的分隔符处切一次）。非法 → InvalidRequest。
fn parse_cursor(tok: &str) -> Result<(f64, GlobalId)> {
    let bad = || {
        EngineError::Core(fastsearch_core::CoreError::InvalidRequest(
            "invalid search_after cursor".into(),
        ))
    };
    if tok.len() < 18 || tok.as_bytes()[16] != b':' {
        return Err(bad());
    }
    let bits = u64::from_str_radix(&tok[..16], 16).map_err(|_| bad())?;
    let gid = GlobalId::parse(&tok[17..])?;
    Ok((f64::from_bits(bits), gid))
}

/// `resolve_citation` 的结果：如何**安全地**取到这段媒资（已过 ACL）。
#[derive(Debug, Clone)]
pub struct ResolvedAsset {
    pub fetch: AssetFetch,
    pub time: Option<TimeSpan>,
    pub media_type: Option<String>,
}

/// 取媒资字节的方式（`resolve_citation` 只定位、不取字节；inline 字节经 [`Engine::fetch_inline_bytes`] 按需取）。
#[derive(Debug, Clone)]
pub enum AssetFetch {
    /// inline 小图：字节在 PG `media_bytes` 真源、**此 cid 可取**（已过 ACL）。字节由网关按需经
    /// `fetch_inline_bytes` 取（不随 resolve 取，省一次 PG 读；也便于签发短时 URL，MM6-signer）。
    InlineRef,
    /// 对象存储**短时签名 URL**（由 [`ObjectSigner`] 签发；绝不含裸 key，不变量 #3）。
    SignedUrl { url: String, expires_s: u64 },
    /// 无独立字节：跳转到原文位置（page+bbox），答案层据此深链/高亮。
    DocRender {
        doc_id: String,
        page: u32,
        bbox: BBox,
    },
}

/// 对象存储签名器（MM6-secure）：把 `AssetPointer::Object` 的存储 key/uri 签成**短时 URL**，
/// 使客户端能取大媒资字节而**不暴露裸 key**（不变量 #3）。返回 `(签名 URL, 过期秒数)`；
/// `None` = 不可签（如 key 非法）→ 网关 404。真实现（S3 presign 类）属对象存储接入，gated。
pub trait ObjectSigner: Send + Sync {
    fn sign(&self, cid: &str, uri: &str, media_type: Option<&str>) -> Option<(String, u64)>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectErrorKind {
    NotFound,
    Forbidden,
    Transient,
    InvalidMetadata,
    TooLarge,
    UnsupportedMediaType,
}

#[derive(Debug, Clone)]
pub struct ObjectError {
    pub kind: ObjectErrorKind,
    pub message: String,
}

impl std::fmt::Display for ObjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ObjectError {}

pub type ObjectResult<T> = std::result::Result<T, ObjectError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub uri: String,
    pub bucket: String,
    pub key: String,
    pub provider: String,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    pub etag: Option<String>,
    pub owner_tenant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBytes {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignedUrl {
    pub url: String,
    pub expires_s: u64,
}

/// 对象存储后端 trait（P4 地基）。默认构建只编译 trait；S3/MinIO 真实现必须 feature-gated。
pub trait ObjectStore: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> ObjectResult<ObjectRef>;
    fn get(&self, uri: &str, max_bytes: usize) -> ObjectResult<ObjectBytes>;
    fn presign_get(&self, uri: &str, content_type: Option<&str>) -> ObjectResult<PresignedUrl>;
    fn validate_ref(&self, uri: &str, principal_tenant: Option<&str>) -> ObjectResult<ObjectRef>;
    fn delete(&self, uri: &str) -> ObjectResult<()>;
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn parse_object_uri(uri: &str) -> ObjectResult<(&str, &str, &str)> {
    let Some((provider, rest)) = uri.split_once("://") else {
        return Err(ObjectError {
            kind: ObjectErrorKind::InvalidMetadata,
            message: "object uri must be scheme://bucket/key".into(),
        });
    };
    if provider != "s3" && provider != "minio" && provider != "local" {
        return Err(ObjectError {
            kind: ObjectErrorKind::InvalidMetadata,
            message: format!("unsupported object uri scheme: {provider}"),
        });
    }
    let Some((bucket, key)) = rest.split_once('/') else {
        return Err(ObjectError {
            kind: ObjectErrorKind::InvalidMetadata,
            message: "object uri must include bucket and key".into(),
        });
    };
    if bucket.is_empty()
        || bucket == "."
        || bucket == ".."
        || bucket.contains('/')
        || bucket.contains('\\')
        || bucket.chars().any(char::is_whitespace)
        || key.is_empty()
        || key
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || key.contains('\\')
        || key.starts_with('/')
    {
        return Err(ObjectError {
            kind: ObjectErrorKind::InvalidMetadata,
            message: "invalid bucket or key".into(),
        });
    }
    Ok((provider, bucket, key))
}

/// 本地目录对象存储。它保留 S3-compatible 的 `s3://bucket/key` / `minio://bucket/key`
/// URI 语义，用于默认构建和本地 MinIO 替身测试；真实 SDK 后端可实现同一个 trait。
#[derive(Debug, Clone)]
pub struct LocalObjectStore {
    root: PathBuf,
    default_bucket: String,
    max_bytes: usize,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>, default_bucket: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            default_bucket: default_bucket.into(),
            max_bytes: 20 * 1024 * 1024,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    fn path_for(&self, bucket: &str, key: &str) -> PathBuf {
        let mut p = self.root.join(bucket);
        for part in key.split('/') {
            p.push(part);
        }
        p
    }

    fn read_ref(&self, provider: &str, bucket: &str, key: &str) -> ObjectResult<ObjectRef> {
        let path = self.path_for(bucket, key);
        let meta = std::fs::metadata(&path).map_err(|e| ObjectError {
            kind: if e.kind() == std::io::ErrorKind::NotFound {
                ObjectErrorKind::NotFound
            } else {
                ObjectErrorKind::Transient
            },
            message: format!("stat object: {e}"),
        })?;
        if meta.len() as usize > self.max_bytes {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: format!("object is {} bytes, limit {}", meta.len(), self.max_bytes),
            });
        }
        let bytes = std::fs::read(&path).map_err(|e| ObjectError {
            kind: ObjectErrorKind::Transient,
            message: format!("read object: {e}"),
        })?;
        Ok(ObjectRef {
            uri: format!("{provider}://{bucket}/{key}"),
            bucket: bucket.to_string(),
            key: key.to_string(),
            provider: provider.to_string(),
            content_type: content_type_for_key(key).to_string(),
            size: meta.len(),
            sha256: sha256_hex(&bytes),
            etag: None,
            owner_tenant: None,
        })
    }

    fn validate_scope(
        &self,
        bucket: &str,
        key: &str,
        principal_tenant: Option<&str>,
    ) -> ObjectResult<()> {
        validate_object_scope(bucket, key, &self.default_bucket, principal_tenant)
    }
}

fn validate_object_scope(
    bucket: &str,
    key: &str,
    allowed_bucket: &str,
    principal_tenant: Option<&str>,
) -> ObjectResult<()> {
    if bucket != allowed_bucket {
        return Err(ObjectError {
            kind: ObjectErrorKind::Forbidden,
            message: "object bucket is not allowed".into(),
        });
    }
    if let Some(tenant) = principal_tenant {
        let prefix = format!("{tenant}/");
        if !key.starts_with(&prefix) {
            return Err(ObjectError {
                kind: ObjectErrorKind::Forbidden,
                message: "object key is outside tenant namespace".into(),
            });
        }
    }
    Ok(())
}

fn content_type_for_key(key: &str) -> &'static str {
    match key
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

impl ObjectStore for LocalObjectStore {
    fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> ObjectResult<ObjectRef> {
        if bytes.len() > self.max_bytes {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: format!("object is {} bytes, limit {}", bytes.len(), self.max_bytes),
            });
        }
        if key.is_empty() || key.split('/').any(|p| p == "..") || key.starts_with('/') {
            return Err(ObjectError {
                kind: ObjectErrorKind::InvalidMetadata,
                message: "invalid object key".into(),
            });
        }
        let path = self.path_for(&self.default_bucket, key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ObjectError {
                kind: ObjectErrorKind::Transient,
                message: format!("create object parent: {e}"),
            })?;
        }
        std::fs::write(&path, bytes).map_err(|e| ObjectError {
            kind: ObjectErrorKind::Transient,
            message: format!("write object: {e}"),
        })?;
        Ok(ObjectRef {
            uri: format!("s3://{}/{key}", self.default_bucket),
            bucket: self.default_bucket.clone(),
            key: key.to_string(),
            provider: "s3".into(),
            content_type: content_type.to_string(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            etag: None,
            owner_tenant: None,
        })
    }

    fn get(&self, uri: &str, max_bytes: usize) -> ObjectResult<ObjectBytes> {
        let (_, bucket, key) = parse_object_uri(uri)?;
        let limit = max_bytes.min(self.max_bytes).max(1);
        let path = self.path_for(bucket, key);
        let meta = std::fs::metadata(&path).map_err(|e| ObjectError {
            kind: if e.kind() == std::io::ErrorKind::NotFound {
                ObjectErrorKind::NotFound
            } else {
                ObjectErrorKind::Transient
            },
            message: format!("stat object: {e}"),
        })?;
        if meta.len() as usize > limit {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: format!("object is {} bytes, limit {limit}", meta.len()),
            });
        }
        let bytes = std::fs::read(&path).map_err(|e| ObjectError {
            kind: ObjectErrorKind::Transient,
            message: format!("read object: {e}"),
        })?;
        Ok(ObjectBytes {
            bytes,
            content_type: Some(content_type_for_key(key).to_string()),
            size: meta.len(),
        })
    }

    fn presign_get(&self, _uri: &str, _content_type: Option<&str>) -> ObjectResult<PresignedUrl> {
        Err(ObjectError {
            kind: ObjectErrorKind::Forbidden,
            message: "presign is provided by server token signer".into(),
        })
    }

    fn validate_ref(&self, uri: &str, principal_tenant: Option<&str>) -> ObjectResult<ObjectRef> {
        let (provider, bucket, key) = parse_object_uri(uri)?;
        self.validate_scope(bucket, key, principal_tenant)?;
        self.read_ref(provider, bucket, key)
    }

    fn delete(&self, uri: &str) -> ObjectResult<()> {
        let (_, bucket, key) = parse_object_uri(uri)?;
        std::fs::remove_file(self.path_for(bucket, key)).map_err(|e| ObjectError {
            kind: if e.kind() == std::io::ErrorKind::NotFound {
                ObjectErrorKind::NotFound
            } else {
                ObjectErrorKind::Transient
            },
            message: format!("delete object: {e}"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct S3ObjectStore {
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    max_bytes: usize,
}

impl S3ObjectStore {
    pub fn new(
        endpoint: impl Into<String>,
        region: impl Into<String>,
        bucket: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            region: region.into(),
            bucket: bucket.into(),
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            max_bytes: 20 * 1024 * 1024,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    fn url(&self, bucket: &str, key: &str) -> String {
        format!("{}/{}/{}", self.endpoint, bucket, pct_path(key))
    }

    fn host(&self) -> String {
        self.endpoint
            .strip_prefix("https://")
            .or_else(|| self.endpoint.strip_prefix("http://"))
            .unwrap_or(&self.endpoint)
            .to_string()
    }

    fn signed(
        &self,
        method: &str,
        bucket: &str,
        key: &str,
        payload_hash: &str,
    ) -> (String, String, String) {
        let (date, amz_date) = sigv4_dates();
        let host = self.host();
        let canonical_uri = format!("/{}/{}", bucket, pct_path(key));
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signing_key = sigv4_key(&self.secret_key, &date, &self.region, "s3");
        let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );
        (auth, amz_date, host)
    }

    fn request(
        &self,
        method: &str,
        bucket: &str,
        key: &str,
        body: Option<&[u8]>,
        content_type: Option<&str>,
    ) -> ObjectResult<ureq::Response> {
        let payload_hash = sha256_hex(body.unwrap_or(&[]));
        let (auth, amz_date, host) = self.signed(method, bucket, key, &payload_hash);
        let url = self.url(bucket, key);
        let mut req = ureq::request(method, &url)
            .set("authorization", &auth)
            .set("host", &host)
            .set("x-amz-date", &amz_date)
            .set("x-amz-content-sha256", &payload_hash);
        if let Some(ct) = content_type {
            req = req.set("content-type", ct);
        }
        let resp = match body {
            Some(bytes) => req.send_bytes(bytes),
            None => req.call(),
        };
        resp.map_err(|e| object_http_error(method, &url, e))
    }
}

impl ObjectStore for S3ObjectStore {
    fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> ObjectResult<ObjectRef> {
        if bytes.len() > self.max_bytes {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: format!("object is {} bytes, limit {}", bytes.len(), self.max_bytes),
            });
        }
        if key.is_empty() || key.starts_with('/') || key.split('/').any(|p| p == "..") {
            return Err(ObjectError {
                kind: ObjectErrorKind::InvalidMetadata,
                message: "invalid object key".into(),
            });
        }
        self.request("PUT", &self.bucket, key, Some(bytes), Some(content_type))?;
        Ok(ObjectRef {
            uri: format!("s3://{}/{key}", self.bucket),
            bucket: self.bucket.clone(),
            key: key.to_string(),
            provider: "s3".into(),
            content_type: content_type.to_string(),
            size: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            etag: None,
            owner_tenant: None,
        })
    }

    fn get(&self, uri: &str, max_bytes: usize) -> ObjectResult<ObjectBytes> {
        let (_, bucket, key) = parse_object_uri(uri)?;
        let resp = self.request("GET", bucket, key, None, None)?;
        let ct = resp
            .header("content-type")
            .map(str::to_string)
            .or_else(|| Some(content_type_for_key(key).to_string()));
        let reader = resp
            .into_reader()
            .take(max_bytes.min(self.max_bytes).saturating_add(1) as u64);
        let mut reader = std::io::BufReader::new(reader);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map_err(|e| ObjectError {
            kind: ObjectErrorKind::Transient,
            message: format!("read response: {e}"),
        })?;
        if bytes.len() > max_bytes.min(self.max_bytes) {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: "object exceeds max bytes".into(),
            });
        }
        Ok(ObjectBytes {
            size: bytes.len() as u64,
            bytes,
            content_type: ct,
        })
    }

    fn presign_get(&self, _uri: &str, _content_type: Option<&str>) -> ObjectResult<PresignedUrl> {
        Err(ObjectError {
            kind: ObjectErrorKind::Forbidden,
            message: "public presign is served by fastsearch token endpoint".into(),
        })
    }

    fn validate_ref(&self, uri: &str, principal_tenant: Option<&str>) -> ObjectResult<ObjectRef> {
        let (provider, bucket, key) = parse_object_uri(uri)?;
        validate_object_scope(bucket, key, &self.bucket, principal_tenant)?;
        let resp = self.request("HEAD", bucket, key, None, None)?;
        let size = resp
            .header("content-length")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if size as usize > self.max_bytes {
            return Err(ObjectError {
                kind: ObjectErrorKind::TooLarge,
                message: format!("object is {size} bytes, limit {}", self.max_bytes),
            });
        }
        Ok(ObjectRef {
            uri: uri.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            provider: provider.to_string(),
            content_type: resp
                .header("content-type")
                .unwrap_or(content_type_for_key(key))
                .to_string(),
            size,
            sha256: String::new(),
            etag: resp.header("etag").map(str::to_string),
            owner_tenant: None,
        })
    }

    fn delete(&self, uri: &str) -> ObjectResult<()> {
        let (_, bucket, key) = parse_object_uri(uri)?;
        self.request("DELETE", bucket, key, None, None).map(|_| ())
    }
}

fn object_http_error(method: &str, url: &str, e: ureq::Error) -> ObjectError {
    match e {
        ureq::Error::Status(404, _) => ObjectError {
            kind: ObjectErrorKind::NotFound,
            message: format!("{method} {url}: not found"),
        },
        ureq::Error::Status(403, _) | ureq::Error::Status(401, _) => ObjectError {
            kind: ObjectErrorKind::Forbidden,
            message: format!("{method} {url}: forbidden"),
        },
        ureq::Error::Status(code, r) if (400..500).contains(&code) => ObjectError {
            kind: ObjectErrorKind::InvalidMetadata,
            message: format!("{method} {url}: status {code}: {}", r.status_text()),
        },
        other => ObjectError {
            kind: ObjectErrorKind::Transient,
            message: format!("{method} {url}: {other}"),
        },
    }
}

fn pct_path(key: &str) -> String {
    key.split('/')
        .map(pct_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn pct_component(s: &str) -> String {
    let mut o = String::new();
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

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::digest::KeyInit;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    hmac_sha256(key, msg)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sigv4_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn sigv4_dates() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    (
        format!("{y:04}{m:02}{d:02}"),
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// 空对象存储：未配置 S3/MinIO 时使用。所有操作明确返回 Forbidden，不泄露对象存在性。
#[derive(Debug, Default)]
pub struct NoopObjectStore;

impl NoopObjectStore {
    fn err(&self) -> ObjectError {
        ObjectError {
            kind: ObjectErrorKind::Forbidden,
            message: "object store not configured".into(),
        }
    }
}

impl ObjectStore for NoopObjectStore {
    fn put(&self, _key: &str, _bytes: &[u8], _content_type: &str) -> ObjectResult<ObjectRef> {
        Err(self.err())
    }

    fn get(&self, _uri: &str, _max_bytes: usize) -> ObjectResult<ObjectBytes> {
        Err(self.err())
    }

    fn presign_get(&self, _uri: &str, _content_type: Option<&str>) -> ObjectResult<PresignedUrl> {
        Err(self.err())
    }

    fn validate_ref(&self, _uri: &str, _principal_tenant: Option<&str>) -> ObjectResult<ObjectRef> {
        Err(self.err())
    }

    fn delete(&self, _uri: &str) -> ObjectResult<()> {
        Err(self.err())
    }
}

/// 端到端检索引擎。
pub struct Engine {
    text: TextIndex,
    vector: VectorStore,
    reranker: Box<dyn Reranker + Send + Sync>,
    /// 可选嵌入后端：设置后，**CDC 应用路径**（`IndexSink::apply_upsert`）会自动嵌入
    /// chunk 正文并写向量索引（None=仅全文）。详见 `set_embedder`。
    embedder: Option<Box<dyn Embedder + Send + Sync>>,
    /// 可选 **pgvector 直查档（B6）**：设置后，向量召回**绕过引擎侧索引、在 PG 跑 ANN**
    /// （filter/ACL 下推 + iterative scan + 精确后过滤）。仅在 **multi-thread tokio runtime** 下可用
    /// （`run` 内 `block_in_place` 桥接同步检索↔异步 PG 查询）。详见 `set_pg_vector`。
    vector_pg: Option<Arc<fastsearch_pg::PgStore>>,
    /// 可选 **真源 PG 句柄**（MM6-inline）：媒资网关 `resolve_citation` 的 `Inline` 路径用它
    /// 按需直查 PG `media_bytes` 字节（字节是真源、引擎派生层不持）。与 `vector_pg` 解耦（语义不同，
    /// 可指向同一 `PgStore`）。仅在 **multi-thread tokio runtime** 下可用（`block_in_place` 桥接）。
    source_pg: Option<Arc<fastsearch_pg::PgStore>>,
    /// 可选对象存储。Object 图片嵌入和 token 字节端点从这里读取；默认未配置。
    object_store: Option<Arc<dyn ObjectStore>>,
    /// 可选 **对象存储签名器**（MM6-secure）：`resolve_citation` 的 `Object` 路径用它签短时 URL；
    /// **未配置 → `Object` 返回 None（404），绝不回退到裸 key**（不变量 #3）。
    object_signer: Option<Box<dyn ObjectSigner>>,
    /// 嵌入来源/版本标记（B6 写穿时写入 PG `embed_model`，供溯源与幂等守卫）。None→`"unknown"`。
    embed_model: Option<String>,
}

impl Engine {
    pub fn create_in_ram(cfg: TextIndexConfig) -> Result<Self> {
        Self::create_in_ram_with(cfg, VectorBackendKind::Brute)
    }

    /// 内存引擎 + 指定向量后端（`Brute` 默认 / `Hnsw` 大规模 opt-in）。
    pub fn create_in_ram_with(cfg: TextIndexConfig, backend: VectorBackendKind) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::create_in_ram(cfg)?,
            vector: VectorStore::new(backend),
            reranker: Box::new(LexicalOverlapReranker),
            embedder: None,
            vector_pg: None,
            source_pg: None,
            object_store: None,
            object_signer: None,
            embed_model: None,
        })
    }

    pub fn open_or_create(dir: &std::path::Path, cfg: TextIndexConfig) -> Result<Self> {
        Ok(Engine {
            text: TextIndex::open_or_create(dir, cfg)?,
            vector: VectorStore::new(VectorBackendKind::Brute),
            reranker: Box::new(LexicalOverlapReranker),
            embedder: None,
            vector_pg: None,
            source_pg: None,
            object_store: None,
            object_signer: None,
            embed_model: None,
        })
    }

    /// 打开**数据目录**下的完整派生索引（落盘恢复），向量后端默认 `Brute`。
    pub fn open(data_dir: &Path, cfg: TextIndexConfig) -> Result<(Self, Lsn)> {
        Self::open_with(data_dir, cfg, VectorBackendKind::Brute)
    }

    /// 同 [`open`](Self::open)，但**首启（无检查点）时用 `default_backend`**；已有检查点则沿用
    /// 其记录的后端（不被 default 覆盖）。`<data>/text` + `vector.bin` + `checkpoint.json`。
    pub fn open_with(
        data_dir: &Path,
        cfg: TextIndexConfig,
        default_backend: VectorBackendKind,
    ) -> Result<(Self, Lsn)> {
        let text_dir = data_dir.join("text");
        std::fs::create_dir_all(&text_dir)
            .map_err(|e| EngineError::Persist(format!("create data dir: {e}")))?;
        let text = TextIndex::open_or_create(&text_dir, cfg)?;
        let cp = Checkpoint::load(data_dir)?;
        // 已有检查点 → 沿用其后端（hnsw params 取自快照本身）；无检查点（首启）→ 用传入默认。
        let kind = match cp.vector_backend.as_str() {
            "hnsw" => VectorBackendKind::Hnsw(fastsearch_vector::HnswParams::default()),
            "brute" => VectorBackendKind::Brute,
            "brute_binary" => {
                VectorBackendKind::BruteBinary(fastsearch_vector::DEFAULT_BINARY_OVERSAMPLE)
            }
            "brute_binary_rotated" => {
                VectorBackendKind::BruteBinaryRotated(fastsearch_vector::DEFAULT_BINARY_OVERSAMPLE)
            }
            _ => default_backend,
        };
        let vector = VectorStore::load(kind, &vector_path(data_dir))
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        // 维度漂移（换了嵌入模型）告警——可见、不静默。
        if let (Some(saved), Some(cur)) = (cp.vector_dim, vector.dim()) {
            if saved != cur {
                eprintln!("warning: checkpoint vector_dim {saved} != loaded {cur}");
            }
        }
        Ok((
            Engine {
                text,
                vector,
                reranker: Box::new(LexicalOverlapReranker),
                embedder: None,
                vector_pg: None,
                source_pg: None,
                object_store: None,
                object_signer: None,
                embed_model: None,
            },
            Lsn(cp.applied_lsn),
        ))
    }

    /// 持久化派生索引 + 检查点（先落盘、后由调用方推进 slot——崩溃安全的前提）：
    /// `text.commit()` → 向量原子落盘 → `checkpoint.json` 原子写入 `applied_lsn`。
    pub fn persist(&mut self, data_dir: &Path, applied_lsn: Lsn) -> Result<()> {
        self.text.commit()?;
        self.vector
            .save(&vector_path(data_dir))
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Checkpoint {
            schema_version: 1,
            applied_lsn: applied_lsn.0,
            vector_dim: self.vector.dim(),
            vector_backend: self.vector.kind_str().to_string(),
        }
        .save(data_dir)?;
        Ok(())
    }

    /// **初始快照 bootstrap**：把已有 PG 行（`(collection, chunk)`）逐条 `apply_upsert`
    /// （经 embedder 嵌入 passage → 写向量索引），再 `persist(data_dir, lsn)`。`lsn` 传 slot
    /// 一致点 → 之后从该 LSN 起增量；幂等保证重叠窗口不产生重复（见
    /// [计划](../../docs/plans/2026-06-25-初始快照-bootstrap.md)）。返回导入条数。
    pub fn bootstrap_snapshot(
        &mut self,
        rows: &[(String, Chunk)],
        data_dir: &Path,
        lsn: Lsn,
    ) -> Result<usize> {
        use fastsearch_sync::IndexSink;
        for (collection, chunk) in rows {
            self.apply_upsert(collection, chunk)
                .map_err(|e| EngineError::Cdc(format!("bootstrap apply: {e}")))?;
        }
        self.persist(data_dir, lsn)?;
        Ok(rows.len())
    }

    /// **单集合原地重建**（坏索引/索引损坏 → 从真源 PG 重灌）：清空派生 text+vector 索引，
    /// 用传入的 `rows`（PG 全表/单集合快照，真源）经 `apply_upsert` 重灌（含嵌入），统一
    /// `commit` 成一次可见切换。**派生可重建**不变量的运维出口；不触 PG，调用方负责 fetch。
    ///
    /// 换分词器属"换 schema"，走另一条路（用新 `TextIndexConfig` 新建 Engine + `bootstrap_snapshot`）——
    /// 本方法保持同 schema。返回重灌条数。
    pub fn rebuild_from(&mut self, rows: &[(String, Chunk)]) -> Result<usize> {
        use fastsearch_sync::IndexSink;
        self.text.clear()?;
        self.vector.clear();
        for (collection, chunk) in rows {
            self.apply_upsert(collection, chunk)
                .map_err(|e| EngineError::Cdc(format!("rebuild apply: {e}")))?;
        }
        self.text.commit()?;
        Ok(rows.len())
    }

    /// 向量后端名（`brute`/`brute_binary`/`brute_binary_rotated`/`hnsw`）。pgvector 直查档
    /// 下后端索引仍为底层暴力档，但 `vector_pg` 已配——见 [`Self::has_pg_vector`]。供 introspection。
    pub fn vector_backend(&self) -> &'static str {
        self.vector.kind_str()
    }

    /// 向量维度（首条 upsert 确定；空库 None）。
    pub fn vector_dim(&self) -> Option<usize> {
        self.vector.dim()
    }

    /// 引擎侧向量条目数（pgvector 直查档下恒 0，向量在 PG）。
    pub fn vector_len(&self) -> usize {
        self.vector.len()
    }

    /// 是否启用了 **pgvector 直查档**（向量召回绕引擎索引、在 PG 跑 ANN）。
    pub fn has_pg_vector(&self) -> bool {
        self.vector_pg.is_some()
    }

    /// **崩溃安全地**消费一批 CDC 变更并落地（生产 CDC 主循环的一拍）：
    /// `peek`（不推进 slot）→ 幂等应用全部（`apply_upsert` 含嵌入）→ `persist`（索引 +
    /// 检查点=slot 高水位）→ **落盘成功后才** `advance_slot`。返回应用条数。
    ///
    /// **不靠 LSN 水位跳过**：`pg_logical_slot_peek` 的逐行 lsn 对一个事务的 Begin/Insert
    /// 报的是事务起点（首事务等于 slot 一致点），用它做水位会误跳首批。正确性靠：① slot
    /// 在 `advance` 前不重投；② 应用按 `GlobalId` upsert/delete **幂等**——崩溃重投同结果。
    /// 故每拍用 `Applier::new(Lsn(0))` 应用全部 peek 到的变更。
    pub async fn consume_once(
        &mut self,
        cfg: &ReplicationConfig,
        data_dir: &Path,
    ) -> Result<usize> {
        let (events, slot_lsn) = peek_with_lsn(cfg)
            .await
            .map_err(|e| EngineError::Cdc(format!("peek: {e}")))?;
        if events.is_empty() {
            // 仅非数据消息推进了 WAL：把 slot 推到已查看最高位，避免空转重读。
            if slot_lsn > Lsn(0) {
                advance_slot(cfg, slot_lsn)
                    .await
                    .map_err(|e| EngineError::Cdc(format!("advance: {e}")))?;
            }
            return Ok(0);
        }
        let mut applier = Applier::new(Lsn(0)); // 不跳过：应用全部（见上）
        let applied = applier
            .apply_batch(self, &events)
            .map_err(|e| EngineError::Cdc(format!("apply: {e}")))?;
        // 先落盘（索引 + 检查点=slot 高水位，含 Commit），后推进 slot —— 崩溃安全铁律。
        self.persist(data_dir, slot_lsn)?;
        advance_slot(cfg, slot_lsn)
            .await
            .map_err(|e| EngineError::Cdc(format!("advance: {e}")))?;
        Ok(applied)
    }

    /// 替换 reranker（接入真 cross-encoder 时用）。
    pub fn set_reranker(&mut self, reranker: Box<dyn Reranker + Send + Sync>) {
        self.reranker = reranker;
    }

    /// 设置嵌入后端：开启后 **CDC 落地（`apply_upsert`）自动嵌入 chunk 正文 → 写向量索引**，
    /// 使"PG 写 → 复制 → 解码 → 嵌入 → 派生 BM25+向量"主循环完整成立。None=仅全文。
    pub fn set_embedder(&mut self, embedder: Box<dyn Embedder + Send + Sync>) {
        self.embedder = Some(embedder);
    }

    /// 开启 **pgvector 直查档（B6）**：向量召回改在 PG 跑 ANN（见字段 `vector_pg`）。
    /// **要求 multi-thread tokio runtime**（检索在 `block_in_place` 里 `block_on` PG 异步查询）。
    /// 仅影响向量召回；keyword 仍走引擎 Tantivy。
    ///
    /// **写穿（B6 续作）**：设此句柄后，CDC 落地路径 `apply_upsert` 会把嵌入**写回 PG `embedding`
    /// 列**（`set_embedding`）而非引擎侧派生索引——直查档读 PG，故写也归 PG，闭环。复制流已排除
    /// `embedding`/`embed_model`/`updated_at`（DDL 列清单 publication）→ 写穿不触发 CDC 反馈环。
    pub fn set_pg_vector(&mut self, store: std::sync::Arc<fastsearch_pg::PgStore>) {
        self.vector_pg = Some(store);
    }

    /// 设置嵌入来源标记（写穿时落 PG `embed_model`，溯源 + 幂等守卫）。server 传嵌入模型名。
    pub fn set_embed_model(&mut self, model: impl Into<String>) {
        self.embed_model = Some(model.into());
    }

    /// 以图搜图查询嵌入（MM9）：`req.query_image` 存在且配了**支持图像**的后端 → 嵌成查询向量；
    /// 无 `query_image`/无 embedder → `None`（不报错，退化为纯文本/无向量）。后端不支持图像 → 报错
    /// （避免静默拿文本后端嵌图）。写入侧只把 `caps.cross_modal=true` 的图片向量放入共享索引，
    /// 避免不可比较的图像向量污染文本向量空间。
    fn embed_query_image(&self, req: &SearchRequest) -> Result<Option<Vec<f32>>> {
        let (Some(bytes), Some(emb)) = (req.query_image.as_ref(), &self.embedder) else {
            return Ok(None);
        };
        if !emb.caps().image {
            return Err(EngineError::Vector(
                "query_image 需要支持图像的嵌入后端（caps.image=false）".into(),
            ));
        }
        if !emb.caps().cross_modal {
            // 非跨模态后端：写入侧模态路由（apply_upsert）从不把图向量放入共享（文本）索引，
            // 故查询图嵌成的图域向量只会与文本向量做无意义余弦。拒绝而非静默返回垃圾命中（M8）。
            return Err(EngineError::Vector(
                "query_image 需要跨模态嵌入后端（caps.cross_modal=false：图向量不与索引里的文本向量同空间）".into(),
            ));
        }
        let v = emb
            .embed_multi(&[EmbedInput::Image(bytes.clone())], EmbedKind::Query)
            .map_err(|e| EngineError::Vector(e.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| EngineError::Vector("image embedder returned no vector".into()))?;
        Ok(Some(v))
    }

    /// 开启 **媒资真源直查（MM6-inline）**：`resolve_citation` 的 `Inline` 路径据此从 PG
    /// `media_bytes` 按需取字节（字节是真源、引擎派生层不持）。可与 `set_pg_vector` 共用同一
    /// `PgStore`。**要求 multi-thread tokio runtime**（`resolve_citation` 内 `block_in_place` 桥接）。
    pub fn set_source_store(&mut self, store: Arc<fastsearch_pg::PgStore>) {
        self.source_pg = Some(store);
    }

    /// Clone 出 source_pg 的 Arc（不耗所有权；server 端 `/v1/index` 用于回写真源 PG）。
    /// PgStore 内部串行化写事务，所以调用方无需重新连接或独占 Arc。
    pub fn source_pg_clone(&self) -> Option<Arc<fastsearch_pg::PgStore>> {
        self.source_pg.clone()
    }

    /// 取 inline 媒资字节（`AssetFetch::InlineRef` 的字节面，MM6-inline/signer）。从 PG `media_bytes`
    /// 真源按需直查（block_in_place 桥接异步 PG，要求 multi-thread runtime）。
    ///
    /// **不带 ACL**：调用方须已授权——authed 网关先过 `resolve_citation` 的 ACL，或 token 端点已验签
    /// （token 即"已授权"凭证，MM6-signer）。**仅供这些已授权出口内部调用，勿新挂别的入口。**
    /// 无 `source_pg` / cid 非法 / 无字节 → `Ok(None)`（网关 404）。
    pub fn fetch_inline_bytes(&self, cid: &str) -> Result<Option<Vec<u8>>> {
        let gid = match GlobalId::parse(cid) {
            Ok(g) => g,
            Err(_) => return Ok(None),
        };
        match &self.source_pg {
            Some(pg) => {
                let bytes = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(pg.fetch_media_bytes(
                        &gid.collection,
                        &gid.doc_id,
                        gid.chunk_id,
                    ))
                })
                .map_err(|e| EngineError::Vector(format!("fetch media_bytes: {e}")))?;
                Ok(bytes)
            }
            None => Ok(None),
        }
    }

    /// 配置对象存储。Object 图片嵌入和 token 字节端点都会从这里取字节。
    pub fn set_object_store(&mut self, store: Arc<dyn ObjectStore>) {
        self.object_store = Some(store);
    }

    /// 已授权的 Object 字节读取。调用方必须已通过 ACL 或 token 验证。
    pub fn fetch_object_bytes(&self, uri: &str, max_bytes: usize) -> Result<Option<ObjectBytes>> {
        match &self.object_store {
            Some(store) => match store.get(uri, max_bytes) {
                Ok(obj) => Ok(Some(obj)),
                Err(e) if e.kind == ObjectErrorKind::NotFound => Ok(None),
                Err(e) => Err(EngineError::Vector(format!("fetch object bytes: {e}"))),
            },
            None => Ok(None),
        }
    }

    pub fn put_object(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<ObjectRef> {
        let Some(store) = &self.object_store else {
            return Err(EngineError::Vector("object store not configured".into()));
        };
        store
            .put(key, bytes, content_type)
            .map_err(|e| EngineError::Vector(format!("put object: {e}")))
    }

    pub fn validate_object_ref(
        &self,
        uri: &str,
        principal_tenant: Option<&str>,
    ) -> Result<ObjectRef> {
        let Some(store) = &self.object_store else {
            return Err(EngineError::Vector("object store not configured".into()));
        };
        store
            .validate_ref(uri, principal_tenant)
            .map_err(|e| EngineError::Vector(format!("validate object ref: {e}")))
    }

    pub fn delete_object(&self, uri: &str) -> Result<()> {
        let Some(store) = &self.object_store else {
            return Ok(());
        };
        match store.delete(uri) {
            Ok(()) => Ok(()),
            Err(e) if e.kind == ObjectErrorKind::NotFound => Ok(()),
            Err(e) => Err(EngineError::Vector(format!("delete object: {e}"))),
        }
    }

    pub fn object_uris_for_doc(&self, collection: &str, doc_id: &str) -> Result<Vec<String>> {
        let rows = self.text.stored_rows_by_doc(collection, doc_id)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(media) = row.media {
                if let AssetPointer::Object { uri } = media.asset {
                    if !out.contains(&uri) {
                        out.push(uri);
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn doc_visible_for_delete(
        &self,
        collection: &str,
        doc_id: &str,
        acl: Option<&AclFilter>,
    ) -> Result<Option<bool>> {
        let rows = self.text.stored_rows_by_doc(collection, doc_id)?;
        if rows.is_empty() {
            return Ok(None);
        }
        Ok(Some(match acl {
            Some(a) => rows.iter().all(|row| a.visible(row)),
            None => true,
        }))
    }

    pub fn object_uri_for_gid(&self, gid: &GlobalId) -> Result<Option<String>> {
        let row = self.text.stored_row_by_gid(gid)?;
        Ok(row.and_then(|r| {
            r.media.and_then(|m| match m.asset {
                AssetPointer::Object { uri } => Some(uri),
                _ => None,
            })
        }))
    }

    fn chunk_image_bytes(&self, chunk: &Chunk) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(bytes) = &chunk.media_bytes {
            return Ok(Some(bytes.clone()));
        }
        let Some(media) = &chunk.media else {
            return Ok(None);
        };
        let AssetPointer::Object { uri } = &media.asset else {
            return Ok(None);
        };
        let Some(store) = &self.object_store else {
            return Ok(None);
        };
        match store.get(uri, 20 * 1024 * 1024) {
            Ok(obj) => Ok(Some(obj.bytes)),
            Err(e)
                if matches!(
                    e.kind,
                    ObjectErrorKind::NotFound | ObjectErrorKind::Forbidden
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(anyhow::anyhow!("object get: {e}")),
        }
    }

    /// 配置 **对象存储签名器（MM6-secure）**：`resolve_citation` 的 `Object` 路径据此签短时 URL。
    /// **不配置则 `Object` 一律 404（不暴露裸 key，不变量 #3）**。真实现（S3 presign 类）gated 对象存储。
    pub fn set_object_signer(&mut self, signer: Box<dyn ObjectSigner>) {
        self.object_signer = Some(signer);
    }

    /// 灌入一个 chunk（仅全文，不提交）。
    pub fn ingest(&mut self, collection: &str, chunk: &Chunk) -> Result<()> {
        self.text.upsert(collection, chunk)?;
        Ok(())
    }

    /// 灌入一个 chunk + 其向量（全文 + 向量索引，不提交）。
    pub fn ingest_vector(
        &mut self,
        collection: &str,
        chunk: &Chunk,
        vector: Vec<f32>,
    ) -> Result<()> {
        self.text.upsert(collection, chunk)?;
        self.vector
            .upsert(
                chunk.global_id(collection),
                vector,
                vec_meta(collection, chunk),
            )
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    /// 删除一个 chunk（全文 + 向量，不提交）。
    pub fn remove(&mut self, gid: &GlobalId) -> Result<()> {
        self.text.delete_by_global_id(gid)?;
        self.vector
            .delete(gid)
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    /// 删除某 doc 全部 chunk（全文 + 向量，不提交）。
    pub fn remove_doc(&mut self, collection: &str, doc_id: &str) -> Result<()> {
        self.text.delete_by_doc(collection, doc_id)?;
        self.vector
            .delete_doc(collection, doc_id)
            .map_err(|e| EngineError::Vector(e.to_string()))?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.text.commit()?;
        Ok(())
    }

    /// 排序管线检索：ACL 强制注入（text/vector 各自落实，不可绕过）→ keyword∥semantic
    /// 召回 → core::fuse 融合 → 组装带引用命中。
    ///
    /// - mode=Keyword：仅全文。mode=Vector：仅向量（需 `req.vector`）。
    /// - mode=Hybrid：两路并行 + 融合（无 `req.vector` 时退化为全文）。
    /// - `fuse` 自带"一路空退化"，故统一调用。
    pub fn search(&self, req: &SearchRequest, acl: Option<&AclFilter>) -> Result<Vec<SearchHit>> {
        Ok(self.run(req, acl)?.0)
    }

    /// 同 [`Engine::search`]，外加按 `req.facets` 计算分面计数（当前支持 `kind`/`doc_id`）。
    pub fn search_with_facets(
        &self,
        req: &SearchRequest,
        acl: Option<&AclFilter>,
    ) -> Result<(Vec<SearchHit>, Facets)> {
        self.run(req, acl)
    }

    /// 由 `citation_id` + ACL 解析"如何安全取到这段媒资"。**ACL 在此强制**：解析出的
    /// chunk 不可见 → 返回 `None`（等同 404，不暴露存在性）。无媒资/Inline 字节不在引擎
    /// （在 PG `media_bytes`，MM2）→ `None`。详见多模态计划 §6。
    pub fn resolve_citation(
        &self,
        cid: &str,
        acl: Option<&AclFilter>,
    ) -> Result<Option<ResolvedAsset>> {
        let gid = GlobalId::parse(cid)?;
        let row = match self.text.stored_row_by_gid(&gid)? {
            Some(r) => r,
            None => return Ok(None),
        };
        // ACL 强制注入（不可绕过）：不可见 → None，等同 404。
        if let Some(a) = acl {
            if !a.visible(&row) {
                return Ok(None);
            }
        }
        let media = match row.media {
            Some(m) => m,
            None => return Ok(None), // 该 chunk 无媒资
        };
        let media_type = media.media_type.clone();
        let fetch = match media.asset {
            AssetPointer::DocRegion { page, bbox } => AssetFetch::DocRender {
                doc_id: row.doc_id,
                page,
                bbox,
            },
            // Object 大媒资在对象存储：必须经签名器签短时 URL（绝不暴露裸 key，不变量 #3）。
            // 无签名器 → None（404），**不回退到裸 uri**。真签名器（S3 presign 类）gated 对象存储。
            AssetPointer::Object { uri } => match &self.object_signer {
                Some(signer) => match signer.sign(cid, &uri, media_type.as_deref()) {
                    Some((url, expires_s)) => AssetFetch::SignedUrl { url, expires_s },
                    None => return Ok(None),
                },
                None => return Ok(None),
            },
            // Inline 字节在 PG media_bytes 真源（MM6-inline）：resolve 只**定位**（ACL 已在上方强制）、
            // 不取字节——返回 InlineRef，字节由网关按需经 `fetch_inline_bytes` 取（省一次 PG 读，
            // 也便于签发短时 URL）。无真源句柄 → 无从取字节 → None（等同 404）。
            AssetPointer::Inline => match &self.source_pg {
                Some(_) => AssetFetch::InlineRef,
                None => return Ok(None),
            },
        };
        Ok(Some(ResolvedAsset {
            fetch,
            time: media.time,
            media_type,
        }))
    }

    /// more_like_this：以种子 chunk 的正文反查相似命中（keyword 模式），排除种子自身。
    /// 种子不存在**或对调用者不可见** → 返回空。ACL 照常强制（不可绕过）。
    pub fn more_like_this(
        &self,
        gid: &GlobalId,
        top_k: usize,
        acl: Option<&AclFilter>,
    ) -> Result<Vec<SearchHit>> {
        // 种子可见性强制（M19）：先取种子行做 ACL 校验——种子对调用者不可见（跨租户/无权）时
        // 当作不存在返回空，绝不以其机密正文构造 MLT 查询、也不泄露其存在性。对齐 resolve_citation。
        let seed_row = match self.text.stored_row_by_gid(gid)? {
            Some(r) => r,
            None => return Ok(vec![]),
        };
        if acl.is_some_and(|a| !a.visible(&seed_row)) {
            return Ok(vec![]);
        }
        let seed_text = match self.text.stored_text(gid)? {
            Some(t) => t,
            None => return Ok(vec![]),
        };
        let query = mlt_query(&seed_text);
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let req = SearchRequest {
            query,
            mode: SearchMode::Keyword,
            top_k: top_k + 1, // 多取一条，扣掉种子自身
            candidates: (top_k + 1).max(SearchRequest::default().candidates),
            ..Default::default()
        };
        let mut hits = self.run(&req, acl)?.0;
        hits.retain(|h| &h.id != gid);
        hits.truncate(top_k);
        Ok(hits)
    }

    fn run(
        &self,
        req: &SearchRequest,
        acl: Option<&AclFilter>,
    ) -> Result<(Vec<SearchHit>, Facets)> {
        req.validate()?;
        let candidates = req.candidates.max(req.top_k);
        if req.vector.is_some()
            && req.query_image.is_none()
            && !req.query.trim().is_empty()
            && filter_targets_image(req.filter.as_ref())
            && self
                .embedder
                .as_ref()
                .is_some_and(|emb| !emb.caps().cross_modal)
        {
            return Err(EngineError::Vector(
                "text-to-image vector search requires caps.cross_modal=true".into(),
            ));
        }

        // 查询向量：显式 `req.vector` 优先；否则若带 `query_image` 且后端支持图像 → 嵌图
        // （**以图搜图**，MM9）。写入侧要求 `caps.cross_modal=true` 才把图片向量放入共享索引，
        // 因此真实图文/图图向量检索都依赖同空间多模态后端。
        let query_vec: Option<Vec<f32>> = match &req.vector {
            Some(v) => Some(v.clone()),
            None => self.embed_query_image(req)?,
        };
        // keyword 召回：空 query 在 text 侧退化成 match-all。当有向量查询（以图搜图/显式向量）且 query
        // 为空时跳过 keyword，避免 match-all 前 `candidates` 条无关文档污染融合排名——Hybrid 是 serde
        // 默认模式，客户端只发 query_image 不显式 mode=Vector 也会中招（M7）。
        let image_only = req.query.trim().is_empty() && query_vec.is_some();
        let want_kw = matches!(req.mode, SearchMode::Keyword | SearchMode::Hybrid) && !image_only;
        let want_vec =
            matches!(req.mode, SearchMode::Vector | SearchMode::Hybrid) && query_vec.is_some();

        // keyword 召回
        let kw_hits: Vec<TextHit> = if want_kw {
            self.text.search(
                &req.query,
                req.filter.as_ref(),
                acl,
                candidates,
                req.highlight,
            )?
        } else {
            vec![]
        };
        // semantic 召回（filter-aware，真预过滤）。pgvector 直查档（B6）时绕引擎索引、在 PG 跑 ANN，
        // 并带回 citation（page/bbox，PG SELECT 出来）；否则走引擎侧向量后端。
        let mut pg_citations: HashMap<GlobalId, Citation> = HashMap::new();
        let vec_scored: Vec<Scored> = if !want_vec {
            vec![]
        } else if let Some(pg) = &self.vector_pg {
            let qv = query_vec.as_ref().unwrap();
            // 同步检索↔异步 PG：block_in_place 桥接（要求 multi-thread runtime）。
            let pairs = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(pg.vector_search(
                    qv,
                    candidates,
                    PG_VECTOR_OVER_FETCH,
                    acl,
                    req.filter.as_ref(),
                ))
            })
            .map_err(|e| EngineError::Vector(e.to_string()))?;
            pairs
                .into_iter()
                .map(|(s, c)| {
                    pg_citations.insert(s.id.clone(), c);
                    s
                })
                .collect()
        } else {
            self.vector
                .search_with_ef(
                    query_vec.as_ref().unwrap(),
                    candidates,
                    req.filter.as_ref(),
                    acl,
                    req.ef_search,
                )
                .map_err(|e| EngineError::Vector(e.to_string()))?
        };

        // 分面：在（keyword）候选集上按字段计数（kind / doc_id）。
        let facets = compute_facets(&req.facets, &kw_hits);

        // 查找表：引用 / 各路分
        let mut kw_score: HashMap<GlobalId, f32> = HashMap::new();
        let mut citation: HashMap<GlobalId, Citation> = HashMap::new();
        let mut highlight: HashMap<GlobalId, String> = HashMap::new();
        let mut text_map: HashMap<GlobalId, String> = HashMap::new();
        for h in &kw_hits {
            kw_score.insert(h.id.clone(), h.score);
            citation.insert(h.id.clone(), h.citation.clone());
            text_map.insert(h.id.clone(), h.text.clone());
            if let Some(hl) = &h.highlight {
                highlight.insert(h.id.clone(), hl.clone());
            }
        }
        let mut vec_score: HashMap<GlobalId, f64> = HashMap::new();
        for s in &vec_scored {
            vec_score.insert(s.id.clone(), s.score);
            citation.entry(s.id.clone()).or_insert_with(|| {
                // pgvector 直查档：用 PG 带回的真实引用；否则引擎侧向量后端；都无则退化占位。
                pg_citations
                    .get(&s.id)
                    .cloned()
                    .or_else(|| self.vector.citation(&s.id))
                    .unwrap_or_else(|| Citation {
                        collection: s.id.collection.clone(),
                        doc_id: s.id.doc_id.clone(),
                        chunk_id: s.id.chunk_id,
                        page: 0,
                        bbox: fastsearch_core::BBox {
                            x0: 0.0,
                            y0: 0.0,
                            x1: 0.0,
                            y1: 0.0,
                        },
                        heading_path: vec![],
                        section_id: 0,
                        time: None,
                        media: None,
                    })
            });
        }

        // 融合（一路空自动退化）
        let kw_list: Vec<Scored> = kw_hits
            .iter()
            .map(|h| Scored {
                id: h.id.clone(),
                score: h.score as f64,
            })
            .collect();
        let fused = fuse(&kw_list, &vec_scored, &req.fusion);

        let mut hits: Vec<SearchHit> = fused
            .into_iter()
            .filter_map(|s| {
                citation.get(&s.id).map(|c| SearchHit {
                    id: s.id.clone(),
                    score: s.score,
                    citation: c.clone(),
                    bm25: kw_score.get(&s.id).copied(),
                    vector: vec_score.get(&s.id).copied(),
                    rerank: None,
                    highlight: highlight.get(&s.id).cloned(),
                    merged_chunk_ids: Vec::new(),
                })
            })
            .collect();

        // auto-merging：同 (doc_id, section_id) 的多个命中片段归并为最高排名的代表，
        // 其余兄弟 chunk_id 记入代表的 merged_chunk_ids 后移除（保序、确定性）。
        // 仅对 section_id != 0（真段）归并；section_id==0 视为"无段"不并。
        if req.auto_merge {
            hits = auto_merge(hits);
        }

        // rerank：宽召回后重排（req.rerank 存在时）。对候选文本打分、按 rerank 分降序、
        // 同分按 gid，再截 top_k。rerank 分写入命中（透明）；原 bm25/vector/fused 保留。
        if let Some(spec) = &req.rerank {
            // rerank 窗口：只对融合分最高的 `rerank.top_k` 个候选重排（hits 此时已是融合序），其余
            // 低分候选丢弃——重排后也进不了最终 top_k。接真 cross-encoder 时这限住延迟/费用（M5）。
            hits.truncate(spec.top_k);
            let texts: Vec<String> = hits
                .iter()
                .map(|h| match text_map.get(&h.id) {
                    Some(t) => t.clone(),
                    // 向量独有命中不在 kw_hits 的 text_map 里，回真源取 STORED 正文再打分；
                    // 否则 rerank 拿空串 → 全候选 0 分、排序退化为 gid 序，向量排名被摧毁（H1-A）。
                    None => self
                        .text
                        .stored_text(&h.id)
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                })
                .collect();
            let scores = self
                .reranker
                .rerank(&req.query, &texts)
                .map_err(|e| EngineError::Rerank(e.to_string()))?;
            for (h, sc) in hits.iter_mut().zip(scores) {
                h.rerank = Some(sc);
            }
            hits.sort_by(|a, b| {
                b.rerank
                    .partial_cmp(&a.rerank)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        // 分组折叠：按最终排名，每组（doc_id/section_id）至多保留 max_per_group 条。
        if let Some(c) = &req.collapse {
            hits = collapse_groups(hits, &c.field, c.max_per_group);
        }
        // 深分页：只保留最终排名中**严格在游标之后**的命中（与 (排序键 desc, gid asc) 一致：
        // 分更低，或同分而 gid 更大）。深度受 `candidates` 候选窗口约束——游标落在窗口外则
        // 该页可能短/空，加大 `candidates` 可加深（标准 search_after 取舍，诚实记账）。
        if let Some(tok) = &req.search_after {
            let (ck, cgid) = parse_cursor(tok)?;
            hits.retain(|h| {
                let k = h.sort_key();
                k < ck || (k == ck && h.id > cgid)
            });
        }
        hits.truncate(req.top_k);
        Ok((hits, facets))
    }
}

/// 分面结果：字段 → [(值, 计数)]（按计数降序、值升序，确定性）。
pub type Facets = std::collections::BTreeMap<String, Vec<(String, u64)>>;

fn compute_facets(fields: &[String], hits: &[TextHit]) -> Facets {
    let mut out = Facets::new();
    for field in fields {
        let mut counts: HashMap<String, u64> = HashMap::new();
        for h in hits {
            let val = match field.as_str() {
                "kind" => Some(h.kind.clone()),
                "modality" => Some(
                    fastsearch_core::Modality::of_kind_str(&h.kind)
                        .as_str()
                        .to_string(),
                ),
                "doc_id" => Some(h.id.doc_id.clone()),
                _ => None, // 支持 kind / modality / doc_id
            };
            if let Some(v) = val {
                *counts.entry(v).or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            continue;
        }
        let mut pairs: Vec<(String, u64)> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.insert(field.clone(), pairs);
    }
    out
}

/// 把同 `(doc_id, section_id)`（`section_id != 0`）的多个命中片段归并为组内最高排名的
/// 代表命中：被并入的兄弟 chunk_id 记入代表的 `merged_chunk_ids`（升序去重）后移除。
/// 输入须已按最终排名排序（代表 = 组内首个出现者）；输出保序、确定性。
fn auto_merge(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut out: Vec<SearchHit> = Vec::with_capacity(hits.len());
    // (collection, doc_id, section_id) → out 中代表命中的下标。section_id 是 doc 内编号，键必须含
    // collection+doc_id，否则跨 collection/跨 doc 的同号 section 会被错误归并成一条（M6）。
    let mut rep: HashMap<(String, String, u64), usize> = HashMap::new();
    for h in hits {
        let sec = h.citation.section_id;
        if sec == 0 {
            out.push(h); // 无段，不参与归并
            continue;
        }
        let key = (h.id.collection.clone(), h.id.doc_id.clone(), sec);
        match rep.get(&key) {
            Some(&idx) => {
                // 已有代表：把本命中并入（记录 chunk_id），丢弃本命中。
                let cid = h.id.chunk_id;
                let merged = &mut out[idx].merged_chunk_ids;
                if let Err(pos) = merged.binary_search(&cid) {
                    merged.insert(pos, cid);
                }
            }
            None => {
                rep.insert(key, out.len());
                out.push(h);
            }
        }
    }
    out
}

/// 把种子正文净化成 keyword 查询：剔除 Tantivy 查询元字符（保留字母/数字/CJK/空白），
/// 取前 `MLT_TERMS` 个词。避免长文本/特殊字符破坏 QueryParser。
const MLT_TERMS: usize = 20;
fn mlt_query(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned
        .split_whitespace()
        .take(MLT_TERMS)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 按最终排名折叠分组：每个分组键至多保留 `max_per_group` 条（高分者优先）。
/// `field` 支持 `doc_id` / `section_id`；其他值视为不折叠（原样返回）。保序、确定性。
fn collapse_groups(hits: Vec<SearchHit>, field: &str, max_per_group: usize) -> Vec<SearchHit> {
    if field != "doc_id" && field != "section_id" {
        return hits; // 未知折叠字段：不折叠
    }
    let mut out: Vec<SearchHit> = Vec::with_capacity(hits.len());
    let mut counts: HashMap<String, usize> = HashMap::new();
    for h in hits {
        // 分组键含 collection：doc_id 全局唯一需带 collection；section_id 是 doc 内编号，需带
        // collection+doc_id，否则不同文档的同号 section 互相挤占名额、静默丢命中（M6）。
        // 用 NUL 分隔（collection/doc_id 不含 NUL，无歧义）。
        let key = match field {
            "doc_id" => format!("{}\u{0}{}", h.id.collection, h.id.doc_id),
            _ => format!(
                "{}\u{0}{}\u{0}{}",
                h.id.collection, h.id.doc_id, h.citation.section_id
            ),
        };
        let n = counts.entry(key).or_insert(0);
        if *n < max_per_group {
            *n += 1;
            out.push(h);
        }
    }
    out
}

/// CDC 落地：sync 的变更应用到 text 索引。放在 engine 而非 text，避免 text 反依赖 sync。
impl fastsearch_sync::IndexSink for Engine {
    fn apply_upsert(&mut self, collection: &str, chunk: &Chunk) -> anyhow::Result<()> {
        self.text.upsert(collection, chunk)?;
        // 配了嵌入后端则同步写向量索引（CDC 主循环：复制→解码→嵌入→派生向量）。
        // **模态路由（MM5/MM10）**：① 图片 chunk 且后端支持图像、有 inline 字节、且
        // 文图同空间（cross_modal）→ 图像嵌入；
        // ② 其余有可检索文本（正文/caption/转录）→ 文本嵌入；
        // ③ 都不满足（无文本图、纯文本后端）→ **不嵌**（避免空串退化向量污染 ANN；仍在 BM25 +
        // modality fast field 可按模态召回）。真跨模态文↔图还需 `caps.cross_modal`（HashEmbedder 基线
        // 为 false，仅图→图有意义；文→图待真跨模态模型）。
        if let Some(emb) = &self.embedder {
            let input: Option<EmbedInput> =
                if chunk.kind == ChunkKind::Image && emb.caps().image && emb.caps().cross_modal {
                    self.chunk_image_bytes(chunk)?.map(EmbedInput::Image)
                } else if !chunk.text.trim().is_empty() {
                    Some(EmbedInput::Text(chunk.text.clone()))
                } else if emb.caps().image && emb.caps().cross_modal {
                    self.chunk_image_bytes(chunk)?.map(EmbedInput::Image)
                } else {
                    None
                };
            match input {
                Some(inp) => {
                    let v = emb
                        .embed_multi(std::slice::from_ref(&inp), EmbedKind::Passage)?
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
                    if let Some(pg) = &self.vector_pg {
                        // B6 写穿：pgvector 直查档——嵌入写回 PG `embedding` 列（直查读 PG，故写归 PG）；
                        // 不写引擎侧派生索引（直查模式读路径不用它）。复制流已排除派生列 → 不触发反馈环。
                        let model = self.embed_model.as_deref().unwrap_or("unknown");
                        block_on_pg(pg.set_embedding(
                            collection,
                            &chunk.doc_id,
                            chunk.chunk_id,
                            &v,
                            model,
                        ))
                        .map_err(|e| anyhow::anyhow!("pg set_embedding: {e}"))?;
                    } else {
                        self.vector
                            .upsert(chunk.global_id(collection), v, vec_meta(collection, chunk))
                            .map_err(|e| anyhow::anyhow!("vector upsert: {e}"))?;
                    }
                }
                None if self.vector_pg.is_some() => {
                    // 幂等：无可嵌入内容 → 清 PG `embedding`（设 NULL），避免直查命中残留向量。
                    let pg = self.vector_pg.as_ref().unwrap();
                    block_on_pg(pg.clear_embedding(collection, &chunk.doc_id, chunk.chunk_id))
                        .map_err(|e| anyhow::anyhow!("pg clear_embedding: {e}"))?;
                }
                None => {
                    // 幂等：覆盖更新时若旧版本有向量、新版本无可嵌入内容，删除旧向量避免残留。
                    self.vector.delete(&chunk.global_id(collection))?;
                }
            }
        }
        Ok(())
    }
    fn apply_delete(&mut self, gid: &GlobalId) -> anyhow::Result<()> {
        self.text.delete_by_global_id(gid)?;
        self.vector.delete(gid)?;
        Ok(())
    }
    fn apply_delete_doc(&mut self, collection: &str, doc_id: &str) -> anyhow::Result<()> {
        self.text.delete_by_doc(collection, doc_id)?;
        self.vector.delete_doc(collection, doc_id)?;
        Ok(())
    }
    fn commit(&mut self) -> anyhow::Result<()> {
        self.text.commit()?;
        Ok(())
    }
}

/// 相关性评测闭环（F39）：把 [`GoldenSet`](fastsearch_eval::GoldenSet) 语料灌入引擎、
/// 对每个查询跑**真实检索**、用判定算指标。eval crate 只管指标与门禁、不跑检索（守住
/// 分层）；"跑检索"这步落在 engine 这里。
///
/// CI 回归门禁的用法：固定 golden 集 + 提交的 baseline [`Metrics`](fastsearch_eval::Metrics)
/// → `run()` 算当前指标 → [`assert_no_regression`](fastsearch_eval::assert_no_regression)。
pub mod golden {
    use crate::{Engine, Result};
    use fastsearch_core::{SearchMode, SearchRequest};
    use fastsearch_eval::{evaluate, GoldenSet, Metrics, RankedResults};
    use fastsearch_text::TextIndexConfig;

    /// 把 golden 语料灌入一个内存引擎，对每个查询跑 `mode` 检索取 top-`k`，算指标均值。
    ///
    /// - 确定性：`mode=Keyword` 不需要嵌入，CI 可零重依赖跑（推荐做门禁）。
    /// - `cfg` 决定分词等索引参数（中文 golden 用 `TokenizerKind::Jieba`）。
    /// - 判定 key 非法 citation_id → 返回 [`EngineError::Core`](crate::EngineError::Core)。
    pub fn run(
        set: &GoldenSet,
        cfg: TextIndexConfig,
        mode: SearchMode,
        k: usize,
    ) -> Result<Metrics> {
        let mut engine = Engine::create_in_ram(cfg)?;
        for c in &set.corpus {
            engine.ingest(&set.collection, c)?;
        }
        engine.commit()?;

        let judg = set.judgments()?;
        let mut results = RankedResults::new();
        for q in &set.queries {
            let req = SearchRequest {
                query: q.query.clone(),
                mode,
                top_k: k,
                // candidates 必须 >= top_k（见 SearchRequest::validate）。
                candidates: k.max(SearchRequest::default().candidates),
                ..Default::default()
            };
            let hits = engine.search(&req, None)?;
            results.set(q.query.clone(), hits.into_iter().map(|h| h.id).collect());
        }
        Ok(evaluate(&results, &judg, k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, ChunkKind, FieldValue, Filter};
    use fastsearch_embed::{EmbedCaps, EmbedInput, EmbedKind, Embedder, HashEmbedder};
    use fastsearch_sync::{Applier, Change, ChangeEvent, Lsn};

    struct CrossModalHashEmbedder(HashEmbedder);

    impl CrossModalHashEmbedder {
        fn new(dim: usize) -> Self {
            Self(HashEmbedder::new(dim))
        }
    }

    impl Embedder for CrossModalHashEmbedder {
        fn dim(&self) -> usize {
            self.0.dim()
        }

        fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
            self.0.embed(texts, kind)
        }

        fn embed_multi(
            &self,
            inputs: &[EmbedInput],
            kind: EmbedKind,
        ) -> anyhow::Result<Vec<Vec<f32>>> {
            self.0.embed_multi(inputs, kind)
        }

        fn caps(&self) -> EmbedCaps {
            EmbedCaps {
                cross_modal: true,
                ..self.0.caps()
            }
        }
    }

    fn chunk(doc: &str, id: u64, kind: ChunkKind, text: &str, page: u32) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind,
            text: text.into(),
            page,
            bbox: BBox {
                x0: 1.0,
                y0: 2.0,
                x1: 3.0,
                y1: 4.0,
            },
            heading_path: vec!["第3章".into()],
            section_id: 7,
            char_len: text.chars().count() as u32,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
        }
    }

    fn engine() -> Engine {
        Engine::create_in_ram(TextIndexConfig::default()).unwrap()
    }

    /// 无 source_pg 时 fetch_inline_bytes → None（本环境，无 PG）；非法 cid 亦 None。
    #[test]
    fn fetch_inline_bytes_without_source_pg_is_none() {
        let e = engine();
        assert_eq!(e.fetch_inline_bytes("kb:d.pdf:1").unwrap(), None);
        assert_eq!(e.fetch_inline_bytes("not-a-valid-cid").unwrap(), None);
    }

    fn req(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            ..Default::default()
        }
    }

    fn chunk_sec(doc: &str, id: u64, text: &str, section_id: u64) -> Chunk {
        Chunk {
            section_id,
            ..chunk(doc, id, ChunkKind::Paragraph, text, 1)
        }
    }

    // MM10（apply_upsert 图像路由）+ MM9（query_image 以图搜图）端到端基线（HashEmbedder，确定）。
    #[test]
    fn mm10_image_routing_and_mm9_query_image() {
        use fastsearch_sync::IndexSink;
        let mut e = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        e.set_embedder(Box::new(CrossModalHashEmbedder::new(32))); // caps.image=true + cross_modal=true

        let img = |id: u64, bytes: Vec<u8>| {
            let mut c = chunk("d.pdf", id, ChunkKind::Image, "", id as u32); // text="" 无 caption
            c.media_bytes = Some(bytes);
            c
        };
        let a_bytes = vec![0x89, 0x50, 0x4E, 0x47, 10, 20, 30, 40, 50];
        let a = img(1, a_bytes.clone());
        let b = img(2, vec![0xFF, 0xD8, 0xFF, 0xE0, 99, 98, 97, 96, 95]);
        let c = img(3, vec![0x47, 0x49, 0x46, 0x38, 1, 1, 2, 3, 5, 8]);
        // 无字节的无文本图：应被跳过（不进向量）。
        let no_bytes = img2_no_bytes();
        for ch in [&a, &b, &c, &no_bytes] {
            e.apply_upsert("kb", ch).unwrap();
        }
        e.commit().unwrap();

        // MM9：以图搜图——用 a 的字节做 query_image → 嵌成查询向量 → 最近邻应是图 a 自身（cos=1）。
        let req = SearchRequest {
            mode: SearchMode::Vector,
            query_image: Some(a_bytes),
            ..Default::default()
        };
        let hits = e.search(&req, None).unwrap();
        assert!(!hits.is_empty(), "以图搜图应有结果");
        assert_eq!(
            hits[0].id.chunk_id, 1,
            "query_image=a 字节 → 最近邻是图 a 自身"
        );
        // 无字节无文本图（chunk_id=9）从未写向量 → 不会出现在向量结果。
        assert!(
            hits.iter().all(|h| h.id.chunk_id != 9),
            "无字节无文本图不应进向量召回"
        );
    }

    #[test]
    fn non_cross_modal_image_vectors_are_not_mixed_into_shared_index() {
        use fastsearch_sync::IndexSink;
        let mut e = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        e.set_embedder(Box::new(HashEmbedder::new(32))); // image=true, cross_modal=false
        let mut c = chunk("d.pdf", 1, ChunkKind::Image, "", 1);
        c.media_bytes = Some(vec![0x89, 0x50, 0x4E, 0x47, 10, 20, 30, 40, 50]);
        e.apply_upsert("kb", &c).unwrap();
        e.commit().unwrap();

        // M8：非跨模态后端的 query_image 直接报错（图向量不与索引里的文本向量同空间），
        // 而非静默返回可能的垃圾/空命中——旧行为恰好因"库里无图向量"返回空、遮蔽了洞。
        let err = e.search(
            &SearchRequest {
                mode: SearchMode::Vector,
                query_image: c.media_bytes.clone(),
                top_k: 10,
                candidates: 10,
                ..Default::default()
            },
            None,
        );
        assert!(
            err.is_err(),
            "non-cross-modal query_image 应报错，不与文本向量索引混比"
        );
    }

    #[test]
    fn object_image_bytes_are_embedded_from_object_store() {
        use fastsearch_core::MediaRef;
        use fastsearch_sync::IndexSink;

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalObjectStore::new(tmp.path(), "assets"));
        let bytes = vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4];
        let obj = store.put("kb/img-a/1.png", &bytes, "image/png").unwrap();

        let mut e = engine();
        e.set_object_store(store);
        e.set_embedder(Box::new(CrossModalHashEmbedder::new(32)));
        let mut c = chunk("img-a", 1, ChunkKind::Image, "", 1);
        c.media = Some(MediaRef {
            asset: AssetPointer::Object { uri: obj.uri },
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        e.apply_upsert("kb", &c).unwrap();
        e.commit().unwrap();

        let hits = e
            .search(
                &SearchRequest {
                    query: String::new(),
                    mode: SearchMode::Vector,
                    query_image: Some(bytes),
                    top_k: 3,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(hits[0].id.to_citation_id(), "kb:img-a:1");
    }

    #[test]
    fn local_object_store_rejects_bucket_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalObjectStore::new(tmp.path(), "assets");
        let err = store
            .get("s3://../secret.png", 1024)
            .expect_err("bucket traversal must be rejected");
        assert_eq!(err.kind, ObjectErrorKind::InvalidMetadata);
    }

    fn img2_no_bytes() -> Chunk {
        chunk("d.pdf", 9, ChunkKind::Image, "", 9) // text="" 且 media_bytes=None
    }

    // 纯文本后端 + query_image → 报错（不静默拿文本后端嵌图）。
    #[test]
    fn mm9_query_image_requires_image_backend() {
        let mut e = engine();
        e.set_embedder(Box::new(fastsearch_embed::HashEmbedder::new(16))); // 支持图像 → 不报错路径
                                                                           // 用一个纯文本后端验证报错：构造默认 trait 行为的后端。
        struct TextOnly;
        impl fastsearch_embed::Embedder for TextOnly {
            fn dim(&self) -> usize {
                16
            }
            fn embed(
                &self,
                texts: &[String],
                _: fastsearch_embed::EmbedKind,
            ) -> anyhow::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|_| vec![0.0; 16]).collect())
            }
        }
        e.set_embedder(Box::new(TextOnly));
        let req = SearchRequest {
            mode: SearchMode::Vector,
            query_image: Some(vec![1, 2, 3, 4]),
            ..Default::default()
        };
        let err = e.search(&req, None).unwrap_err().to_string();
        assert!(err.contains("caps.image=false"), "got: {err}");
    }

    #[test]
    fn hnsw_backend_end_to_end() {
        use fastsearch_vector::HnswParams;
        let mut e = Engine::create_in_ram_with(
            TextIndexConfig::default(),
            VectorBackendKind::Hnsw(HnswParams::default()),
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "x", 1),
            vec![1.0, 0.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "y", 1),
            vec![0.0, 1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 3, ChunkKind::Paragraph, "z", 1),
            vec![0.0, 0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(vec![0.9, 0.1, 0.0]),
            ..Default::default()
        };
        let hits = e.search(&r, None).unwrap();
        assert!(!hits.is_empty());
        // 最近 [1,0,0] → chunk 1 居首（HNSW 后端经引擎端到端可用）
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    /// B6 直查档接入引擎（需 DATABASE_URL + multi-thread runtime）：向量召回在 PG 跑 ANN，
    /// 引擎拿到带真实 page/bbox 的引用并完成排序。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pgvector_backend_via_engine() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip pgvector_backend_via_engine: DATABASE_URL not set");
            return;
        };
        use fastsearch_pg::{PgConfig, PgStore, VectorType};
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_eng_vec_it".into();
        cfg.vector_dim = 4;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");
        // 3 个 chunk（page 各异），写正交向量。
        let mk = |id: u64, page: u32| Chunk {
            page,
            ..chunk("d.pdf", id, ChunkKind::Paragraph, &format!("c{id}"), 1)
        };
        let chunks = vec![mk(1, 11), mk(2, 22), mk(3, 33)];
        store
            .upsert_doc("kb", "d.pdf", &chunks)
            .await
            .expect("upsert");
        for id in 1..=3u64 {
            let mut e = vec![0.0f32; 4];
            e[(id - 1) as usize] = 1.0;
            store
                .set_embedding("kb", "d.pdf", id, &e, "test")
                .await
                .expect("emb");
        }

        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_pg_vector(std::sync::Arc::new(store));
        // 查最接近 [0,1,0,0] = chunk 2（page 22）。
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(vec![0.1, 0.9, 0.0, 0.0]),
            ..Default::default()
        };
        let hits = engine.search(&r, None).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].id.chunk_id, 2, "PG ANN 最近邻应为 chunk 2");
        assert_eq!(hits[0].citation.page, 22, "引用 page 应来自 PG（非退化 0）");
        assert!(hits[0].vector.is_some(), "应有向量分");
    }

    /// B6 写穿（需 DATABASE_URL + multi-thread runtime）：CDC 落地 `apply_upsert` 在 pgvector 直查
    /// 模式下把嵌入**写回 PG `embedding` 列**（而非引擎派生索引）；写穿后直查即命中。空文本则清向量。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn b6_cdc_write_through_to_pg_embedding() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip b6_cdc_write_through_to_pg_embedding: DATABASE_URL not set");
            return;
        };
        use fastsearch_embed::{EmbedKind, Embedder, HashEmbedder};
        use fastsearch_pg::{PgConfig, PgStore, VectorType};
        use fastsearch_sync::IndexSink;

        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_b6_wt_it".into();
        cfg.vector_dim = 16;
        cfg.vector_type = VectorType::Vector;
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");

        // 源行（embedding 留 NULL，模拟上游/CDC 投递）。doc 级替换 → 重复跑亦干净。
        let c = chunk("d.pdf", 1, ChunkKind::Paragraph, "毛利率 下降 显著", 7);
        store
            .upsert_doc("kb", "d.pdf", std::slice::from_ref(&c))
            .await
            .expect("upsert");

        let store = std::sync::Arc::new(store);
        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.set_embedder(Box::new(HashEmbedder::new(16)));
        engine.set_embed_model("hash@16");
        engine.set_pg_vector(store.clone());

        // CDC 落地一条 upsert → 应写穿到 PG embedding（不写引擎派生索引）。
        engine.apply_upsert("kb", &c).unwrap();

        // 写穿后：PG 直查（用同 embedder 嵌 query）命中 chunk 1，引用 page 来自 PG。
        let qv = HashEmbedder::new(16)
            .embed(&["毛利率 下降".into()], EmbedKind::Query)
            .unwrap()
            .pop()
            .unwrap();
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(qv),
            ..Default::default()
        };
        let hits = engine.search(&r, None).unwrap();
        assert_eq!(
            hits.first().map(|h| h.id.chunk_id),
            Some(1),
            "写穿后 PG 直查应命中 chunk 1"
        );
        assert_eq!(hits[0].citation.page, 7, "引用 page 应来自 PG 真源");

        // 文本变空（媒资丢 caption）→ 写穿路径清 PG 向量 → 直查不再命中。
        let empty = chunk("d.pdf", 1, ChunkKind::Image, "", 7);
        engine.apply_upsert("kb", &empty).unwrap();
        let hits2 = engine.search(&r, None).unwrap();
        assert!(
            hits2.iter().all(|h| h.id.chunk_id != 1),
            "清向量后 chunk 1 不应再被直查命中"
        );
    }

    /// MM6-inline 集成（需 DATABASE_URL + multi-thread runtime）：媒资网关 `resolve_citation`
    /// 的 `Inline` 路径从 PG `media_bytes` 真源按需取字节；ACL 强制不可绕过。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mm6_inline_serves_bytes_from_source_pg() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skip mm6_inline_serves_bytes_from_source_pg: DATABASE_URL not set");
            return;
        };
        use fastsearch_core::{AssetPointer, MediaRef};
        use fastsearch_pg::{PgConfig, PgStore};
        let mut cfg = PgConfig::new(url);
        cfg.table = "fastsearch_eng_mb_it".into();
        let store = PgStore::connect(cfg).await.expect("connect");
        store.ensure_schema().await.expect("schema");

        // 带 inline 字节的图 chunk（无 caption，text=""），限 team-a 可见。
        let mut c = chunk("d.pdf", 1, ChunkKind::Image, "", 1);
        c.tenant = Some("acme".into());
        c.acl = vec!["team-a".into()];
        c.media = Some(MediaRef {
            asset: AssetPointer::Inline,
            media_type: Some("image/png".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        let bytes = vec![0x89u8, 0x50, 0x4E, 0x47];
        c.media_bytes = Some(bytes.clone());
        store
            .upsert_doc("kb", "d.pdf", &[c.clone()])
            .await
            .expect("upsert");

        let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
        engine.ingest("kb", &c).unwrap(); // 进引擎 text 索引（resolve 取 MediaRef）
        engine.commit().unwrap();
        engine.set_source_store(std::sync::Arc::new(store));

        let acl_ok = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let acl_no = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-b".into()],
        };
        // 授权 → InlineRef（定位，不取字节）+ Content-Type；字节经 fetch_inline_bytes 取。
        let r = engine
            .resolve_citation("kb:d.pdf:1", Some(&acl_ok))
            .unwrap()
            .expect("authorized → Some");
        assert!(
            matches!(r.fetch, AssetFetch::InlineRef),
            "expected InlineRef, got {:?}",
            r.fetch
        );
        assert_eq!(r.media_type.as_deref(), Some("image/png"));
        // 字节面：fetch_inline_bytes 取 PG 真源字节（已授权出口）。
        assert_eq!(
            engine.fetch_inline_bytes("kb:d.pdf:1").unwrap(),
            Some(bytes.clone()),
            "应取 PG 真源字节"
        );
        // 越权 → None（ACL 不可绕过，不暴露存在性/字节）。
        assert!(engine
            .resolve_citation("kb:d.pdf:1", Some(&acl_no))
            .unwrap()
            .is_none());
    }

    #[test]
    fn search_after_tiles_full_ranking() {
        let mut e = engine();
        // 混合：一条高 tf（分更高）+ 多条同分（靠 gid tie-break），覆盖游标的"同分"分支。
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "data data", 1),
        )
        .unwrap();
        for (doc, id) in [
            ("a.pdf", 2),
            ("a.pdf", 3),
            ("a.pdf", 4),
            ("b.pdf", 1),
            ("b.pdf", 2),
        ] {
            e.ingest("kb", &chunk(doc, id, ChunkKind::Paragraph, "data", 1))
                .unwrap();
        }
        e.commit().unwrap();

        let full = e.search(&req("data"), None).unwrap();
        assert_eq!(full.len(), 6);
        let full_ids: Vec<_> = full.iter().map(|h| h.id.clone()).collect();

        // 逐页 size=2 翻完，应无缝平铺完整排名（无重叠、无遗漏）。
        let mut paged: Vec<GlobalId> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let r = SearchRequest {
                query: "data".into(),
                top_k: 2,
                search_after: cursor.clone(),
                ..Default::default()
            };
            let page = e.search(&r, None).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 2);
            cursor = Some(page.last().unwrap().cursor());
            paged.extend(page.iter().map(|h| h.id.clone()));
        }
        assert_eq!(paged, full_ids, "分页平铺应等于完整排名");

        // 用第 3 条的游标取下一页，应正好接续完整排名的第 4 条起。
        let after3 = SearchRequest {
            query: "data".into(),
            top_k: 10,
            search_after: Some(full[2].cursor()),
            ..Default::default()
        };
        let tail: Vec<_> = e
            .search(&after3, None)
            .unwrap()
            .iter()
            .map(|h| h.id.clone())
            .collect();
        assert_eq!(tail, full_ids[3..]);
    }

    #[test]
    fn search_after_rejects_bad_cursor() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "data", 1))
            .unwrap();
        e.commit().unwrap();
        let r = SearchRequest {
            query: "data".into(),
            search_after: Some("not-a-valid-cursor".into()),
            ..Default::default()
        };
        assert!(e.search(&r, None).is_err());
    }

    #[test]
    fn rebuild_from_truth_replaces_index() {
        let mut e = engine();
        // 旧（可能损坏/过期）索引：含一条将被真源剔除的 chunk。
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "old apple", 1),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "stale banana", 1),
        )
        .unwrap();
        e.commit().unwrap();
        assert_eq!(e.search(&req("banana"), None).unwrap().len(), 1);

        // 从真源重灌：a/2 已不在真源，a/1 内容更新。
        let rows = vec![(
            "kb".to_string(),
            chunk("a.pdf", 1, ChunkKind::Paragraph, "new apple cherry", 1),
        )];
        let n = e.rebuild_from(&rows).unwrap();
        assert_eq!(n, 1);
        // 过期 chunk 已消失；重灌内容可检索。
        assert_eq!(e.search(&req("banana"), None).unwrap().len(), 0);
        let hits = e.search(&req("cherry"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn auto_merge_collapses_same_section() {
        let mut e = engine();
        // 同 doc 同 section 3 个片段；另一 section 1 个；另一 doc 同 section 号 1 个。
        e.ingest("kb", &chunk_sec("a.pdf", 1, "data alpha", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 2, "data beta", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 3, "data gamma", 10))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 4, "data delta", 11))
            .unwrap();
        e.ingest("kb", &chunk_sec("b.pdf", 9, "data epsilon", 10))
            .unwrap();
        e.commit().unwrap();

        // 不归并：5 条
        let plain = e.search(&req("data"), None).unwrap();
        assert_eq!(plain.len(), 5);

        // 归并：a.pdf§10 三条→1 代表，a.pdf§11 一条，b.pdf§10 一条 → 共 3 条
        let mut r = req("data");
        r.auto_merge = true;
        let merged = e.search(&r, None).unwrap();
        assert_eq!(merged.len(), 3);
        // 找 a.pdf§10 的代表：应携带另外两个兄弟 chunk_id（升序）
        let rep = merged
            .iter()
            .find(|h| h.id.doc_id == "a.pdf" && h.citation.section_id == 10)
            .unwrap();
        let mut others: Vec<u64> = [1u64, 2, 3]
            .into_iter()
            .filter(|c| *c != rep.id.chunk_id)
            .collect();
        others.sort_unstable();
        assert_eq!(rep.merged_chunk_ids, others);
        // 其余两条不携带归并
        for h in &merged {
            if !(h.id.doc_id == "a.pdf" && h.citation.section_id == 10) {
                assert!(h.merged_chunk_ids.is_empty());
            }
        }
    }

    #[test]
    fn more_like_this_finds_similar_excludes_seed() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                1,
                ChunkKind::Paragraph,
                "machine learning models",
                1,
            ),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                2,
                ChunkKind::Paragraph,
                "learning models tuning",
                2,
            ),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk(
                "a.pdf",
                3,
                ChunkKind::Paragraph,
                "cooking recipes dinner",
                3,
            ),
        )
        .unwrap();
        e.commit().unwrap();
        let seed = GlobalId {
            collection: "kb".into(),
            doc_id: "a.pdf".into(),
            chunk_id: 1,
        };
        let hits = e.more_like_this(&seed, 10, None).unwrap();
        // 不含种子自身
        assert!(hits.iter().all(|h| h.id.chunk_id != 1));
        // chunk 2（共享 learning/models）应命中，chunk 3（无重叠）不该在最前
        assert!(hits.iter().any(|h| h.id.chunk_id == 2));
        assert_eq!(hits[0].id.chunk_id, 2);
        // 种子不存在 → 空
        let missing = GlobalId {
            collection: "kb".into(),
            doc_id: "a.pdf".into(),
            chunk_id: 999,
        };
        assert!(e.more_like_this(&missing, 10, None).unwrap().is_empty());
    }

    #[test]
    fn more_like_this_seed_acl_enforced_no_cross_tenant_leak() {
        // M19 回归：种子 chunk 对调用者不可见时 more_like_this 返回空——不以其机密正文构造查询、
        // 不泄露存在性（否则成跨租户相似性/存在性预言机）。
        let mut e = engine();
        // 种子：租户 acme、标签 team-a（机密）
        let mut seed = chunk(
            "secret.pdf",
            1,
            ChunkKind::Paragraph,
            "machine learning models",
            1,
        );
        seed.tenant = Some("acme".into());
        seed.acl = vec!["team-a".into()];
        e.ingest("kb", &seed).unwrap();
        // 一条 team-b 可见的相似文档（证明"若非 ACL 拦截会有命中"）
        let mut other = chunk(
            "pub.pdf",
            2,
            ChunkKind::Paragraph,
            "learning models tuning",
            2,
        );
        other.tenant = Some("acme".into());
        other.acl = vec!["team-b".into()];
        e.ingest("kb", &other).unwrap();
        e.commit().unwrap();

        let seed_gid = GlobalId {
            collection: "kb".into(),
            doc_id: "secret.pdf".into(),
            chunk_id: 1,
        };
        // 调用者 B（team-b）：看不到种子 → 返回空（不泄露）
        let acl_b = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-b".into()],
        };
        assert!(
            e.more_like_this(&seed_gid, 5, Some(&acl_b))
                .unwrap()
                .is_empty(),
            "种子对 B 不可见 → more_like_this 应返回空，不泄露"
        );
        // 调用者 A（team-a）：能看到种子 → 正常反查相似命中（对照，证明功能未坏）
        let acl_a = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into(), "team-b".into()],
        };
        assert!(
            !e.more_like_this(&seed_gid, 5, Some(&acl_a))
                .unwrap()
                .is_empty(),
            "种子对 A 可见 → 应能反查相似命中"
        );
    }

    #[test]
    fn modality_filter_two_end() {
        let mut e = engine();
        // 同文本不同 kind：image / audio / paragraph(text)
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Image, "data here", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Audio, "data here", 2))
            .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 3, ChunkKind::Paragraph, "data here", 3),
        )
        .unwrap();
        e.commit().unwrap();
        // keyword 路 + modality=image → 仅 chunk 1（text 侧 modality 由 kind 派生后过滤）
        let mut r = req("data");
        r.filter = Some(Filter::Eq(
            "modality".into(),
            FieldValue::Str("image".into()),
        ));
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        // modality=text → 仅 paragraph(chunk 3)
        let mut r2 = req("data");
        r2.filter = Some(Filter::Eq(
            "modality".into(),
            FieldValue::Str("text".into()),
        ));
        let h2 = e.search(&r2, None).unwrap();
        assert_eq!(h2.len(), 1);
        assert_eq!(h2[0].id.chunk_id, 3);
    }

    #[test]
    fn mm5_textless_media_skips_vector() {
        use fastsearch_embed::{EmbedKind, Embedder, HashEmbedder};
        // MM5（M0 路由）：有文本（caption/转录/正文）→ 嵌入写向量；无文本媒资（text=""）→ 不嵌、
        // 不写向量（否则空串塌成退化向量污染 ANN）。无文本图仍在 BM25 + modality fast field
        // （按模态召回由 fastsearch-text `empty_text_and_modality_fast_field` 覆盖）。
        let emb = HashEmbedder::new(16);
        let mut e = engine();
        e.set_embedder(Box::new(HashEmbedder::new(16)));
        let text_c = chunk(
            "a.pdf",
            1,
            ChunkKind::Paragraph,
            "quarterly gross margin",
            1,
        );
        let img_c = chunk("a.pdf", 2, ChunkKind::Image, "", 2); // 无 caption 的图
                                                                // 经 CDC 落地路径（apply_upsert + 嵌入）灌入。
        e.rebuild_from(&[("kb".to_string(), text_c), ("kb".to_string(), img_c)])
            .unwrap();
        // 向量检索：无文本图没有向量 → 永不出现；修复前它会以"前缀塌缩向量"混入。
        let qv = emb
            .embed(&["quarterly gross margin".into()], EmbedKind::Query)
            .unwrap()
            .remove(0);
        let r = SearchRequest {
            query: String::new(),
            mode: SearchMode::Vector,
            vector: Some(qv),
            candidates: 10,
            top_k: 10,
            ..Default::default()
        };
        let hits = e.search(&r, None).unwrap();
        assert!(
            hits.iter().any(|h| h.id.chunk_id == 1),
            "有文本 chunk 应被嵌入并向量召回"
        );
        assert!(
            hits.iter().all(|h| h.id.chunk_id != 2),
            "无文本媒资不应进入向量索引（避免退化向量污染 ANN）"
        );
    }

    #[test]
    fn resolve_citation_enforces_acl() {
        use fastsearch_core::{AssetPointer, MediaRef};
        let mut e = engine();
        let mut c = chunk("a.pdf", 1, ChunkKind::Image, "figure caption", 7);
        c.tenant = Some("acme".into());
        c.acl = vec!["team-a".into()];
        c.media = Some(MediaRef {
            asset: AssetPointer::DocRegion {
                page: 7,
                bbox: c.bbox,
            },
            media_type: Some("image/png".into()),
            time: None,
            region: Some(c.bbox),
            caption_source: None,
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        let cid = "kb:a.pdf:1";
        let authorized = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let unauthorized = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-b".into()],
        };
        // 授权 → DocRender（跳原文页/区域）
        let r = e.resolve_citation(cid, Some(&authorized)).unwrap().unwrap();
        assert_eq!(r.media_type.as_deref(), Some("image/png"));
        assert!(matches!(r.fetch, AssetFetch::DocRender { page: 7, .. }));
        // 越权 → None（等同 404，不暴露存在性）
        assert!(e
            .resolve_citation(cid, Some(&unauthorized))
            .unwrap()
            .is_none());
        // 不存在 → None
        assert!(e
            .resolve_citation("kb:a.pdf:999", Some(&authorized))
            .unwrap()
            .is_none());
    }

    #[test]
    fn mm6_secure_object_no_signer_is_404() {
        use fastsearch_core::{AssetPointer, MediaRef};
        let mut e = engine();
        let mut c = chunk("a.pdf", 1, ChunkKind::Audio, "transcript", 1);
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://bucket/secret-key.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        // 无签名器 → None（404），绝不回退到裸 key（不变量 #3）。
        assert!(
            e.resolve_citation("kb:a.pdf:1", None).unwrap().is_none(),
            "无签名器时 Object 必 404，不暴露裸 key"
        );
    }

    #[test]
    fn mm6_secure_object_with_signer_signs() {
        use fastsearch_core::{AssetPointer, MediaRef};
        struct TestSigner;
        impl ObjectSigner for TestSigner {
            fn sign(
                &self,
                _cid: &str,
                uri: &str,
                _media_type: Option<&str>,
            ) -> Option<(String, u64)> {
                // 真实现会 S3 presign；测试桩只证明：签出的 URL 不含裸 key。
                let name = uri.rsplit('/').next().unwrap_or("asset");
                Some((format!("https://signed.example/{name}?token=abc"), 300))
            }
        }
        let mut e = engine();
        e.set_object_signer(Box::new(TestSigner));
        let mut c = chunk("a.pdf", 1, ChunkKind::Audio, "transcript", 1);
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://bucket/secret-key.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: None,
            region: None,
            caption_source: None,
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        let r = e.resolve_citation("kb:a.pdf:1", None).unwrap().unwrap();
        match r.fetch {
            AssetFetch::SignedUrl { url, expires_s } => {
                assert!(url.starts_with("https://signed.example/"));
                assert!(!url.contains("s3://"), "签名 URL 绝不含裸 key");
                assert_eq!(expires_s, 300);
            }
            other => panic!("expected SignedUrl, got {other:?}"),
        }
    }

    #[test]
    fn media_time_surface_on_citation() {
        use fastsearch_core::{AssetPointer, MediaRef, TimeSpan};
        let mut e = engine();
        // 音频 chunk：转录入 text，media 带时间区间 + 对象指针
        let mut c = chunk("a.pdf", 1, ChunkKind::Audio, "meeting transcript notes", 1);
        c.media = Some(MediaRef {
            asset: AssetPointer::Object {
                uri: "s3://b/clip.mp3".into(),
            },
            media_type: Some("audio/mpeg".into()),
            time: Some(TimeSpan {
                start_ms: 3000,
                end_ms: 8000,
            }),
            region: None,
            caption_source: Some("asr".into()),
            thumbnail: None,
        });
        e.ingest("kb", &c).unwrap();
        e.commit().unwrap();
        // keyword 路（转录命中）→ Citation 透出 time/media
        let hits = e.search(&req("transcript"), None).unwrap();
        assert_eq!(hits.len(), 1);
        let cit = &hits[0].citation;
        assert_eq!(cit.time.unwrap().start_ms, 3000);
        let media = cit.media.as_ref().unwrap();
        assert_eq!(media.media_type.as_deref(), Some("audio/mpeg"));
        assert!(matches!(&media.asset, AssetPointer::Object { uri } if uri == "s3://b/clip.mp3"));
    }

    #[test]
    fn modality_facet_counts() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Image, "data x", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Audio, "data y", 2))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 3, ChunkKind::Paragraph, "data z", 3))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.facets = vec!["modality".into()];
        let (_hits, facets) = e.search_with_facets(&r, None).unwrap();
        let m: std::collections::HashMap<_, _> =
            facets.get("modality").unwrap().iter().cloned().collect();
        assert_eq!(m.get("image"), Some(&1));
        assert_eq!(m.get("audio"), Some(&1));
        assert_eq!(m.get("text"), Some(&1));
    }

    #[test]
    fn collapse_caps_hits_per_doc() {
        let mut e = engine();
        // 3 条 a.pdf + 2 条 b.pdf，都含 "data"
        for id in 1..=3 {
            e.ingest(
                "kb",
                &chunk("a.pdf", id, ChunkKind::Paragraph, "data here", 1),
            )
            .unwrap();
        }
        for id in 1..=2 {
            e.ingest(
                "kb",
                &chunk("b.pdf", id, ChunkKind::Paragraph, "data here", 1),
            )
            .unwrap();
        }
        e.commit().unwrap();
        // 不折叠：5 条
        assert_eq!(e.search(&req("data"), None).unwrap().len(), 5);
        // 折叠 doc_id，每组最多 1 → 2 条（a.pdf 1 + b.pdf 1）
        let mut r = req("data");
        r.collapse = Some(fastsearch_core::Collapse {
            field: "doc_id".into(),
            max_per_group: 1,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 2);
        let docs: std::collections::HashSet<_> = hits.iter().map(|h| h.id.doc_id.clone()).collect();
        assert_eq!(docs.len(), 2);
        // 每组最多 2 → 4 条
        r.collapse = Some(fastsearch_core::Collapse {
            field: "doc_id".into(),
            max_per_group: 2,
        });
        assert_eq!(e.search(&r, None).unwrap().len(), 4);
    }

    #[test]
    fn auto_merge_keeps_section_zero_separate() {
        let mut e = engine();
        // section_id=0 视为"无段"，不应被归并到一起。
        e.ingest("kb", &chunk_sec("a.pdf", 1, "data one", 0))
            .unwrap();
        e.ingest("kb", &chunk_sec("a.pdf", 2, "data two", 0))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.auto_merge = true;
        let merged = e.search(&r, None).unwrap();
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|h| h.merged_chunk_ids.is_empty()));
    }

    #[test]
    fn ingest_search_with_citation() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta", 5),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta gamma", 6),
        )
        .unwrap();
        e.commit().unwrap();
        let hits = e.search(&req("alpha"), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
        assert_eq!(hits[0].citation.page, 5);
        assert!(hits[0].bm25.is_some());
        assert_eq!(hits[0].vector, None);
        assert!(hits[0].highlight.is_none()); // 默认不高亮
    }

    #[test]
    fn highlight_when_requested() {
        let mut e = engine();
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha beta gamma", 5),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("beta");
        r.highlight = true;
        let hits = e.search(&r, None).unwrap();
        assert!(hits[0].highlight.as_ref().unwrap().contains("<b>beta</b>"));
    }

    #[test]
    fn end_to_end_via_cdc_sink() {
        // 端到端：CDC ChangeEvent --Applier--> Engine(IndexSink) --索引--> 检索
        let mut e = engine();
        let mut ap = Applier::new(Lsn(0));
        let evs = vec![
            ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 1, ChunkKind::Table, "毛利率下降", 23)),
                },
                lsn: Lsn(1),
            },
            ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 2, ChunkKind::Paragraph, "新产品发布", 3)),
                },
                lsn: Lsn(2),
            },
        ];
        let n = ap.apply_batch(&mut e, &evs).unwrap();
        assert_eq!(n, 2);
        // jieba 默认分词器为 Default；中文用 Default 分词器命中可能受限，故用整词查询
        let hits = e.search(&req("新产品发布"), None).unwrap();
        assert!(hits.iter().any(|h| h.id.chunk_id == 2));
    }

    #[test]
    fn replace_doc_via_cdc() {
        let mut e = engine();
        let mut ap = Applier::new(Lsn(0));
        // 先灌 chunk 1（oldword）
        ap.apply_batch(
            &mut e,
            &[ChangeEvent {
                change: Change::Upsert {
                    collection: "kb".into(),
                    chunk: Box::new(chunk("a.pdf", 1, ChunkKind::Paragraph, "oldword", 1)),
                },
                lsn: Lsn(1),
            }],
        )
        .unwrap();
        // doc 级替换：DeleteDoc + 新 chunk（newword）
        ap.apply_batch(
            &mut e,
            &[
                ChangeEvent {
                    change: Change::DeleteDoc {
                        collection: "kb".into(),
                        doc_id: "a.pdf".into(),
                    },
                    lsn: Lsn(2),
                },
                ChangeEvent {
                    change: Change::Upsert {
                        collection: "kb".into(),
                        chunk: Box::new(chunk("a.pdf", 9, ChunkKind::Paragraph, "newword", 1)),
                    },
                    lsn: Lsn(3),
                },
            ],
        )
        .unwrap();
        assert_eq!(e.search(&req("oldword"), None).unwrap().len(), 0);
        assert_eq!(e.search(&req("newword"), None).unwrap().len(), 1);
    }

    #[test]
    fn acl_enforced_in_engine() {
        let mut e = engine();
        let mut c1 = chunk("a.pdf", 1, ChunkKind::Paragraph, "secret", 1);
        c1.tenant = Some("acme".into());
        c1.acl = vec!["team-a".into()];
        let mut c2 = chunk("a.pdf", 2, ChunkKind::Paragraph, "secret", 1);
        c2.tenant = Some("acme".into());
        c2.acl = vec!["team-b".into()];
        e.ingest("kb", &c1).unwrap();
        e.ingest("kb", &c2).unwrap();
        e.commit().unwrap();
        let acl = AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let hits = e.search(&req("secret"), Some(&acl)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn filter_applied() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data", 12))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Paragraph, "data", 12))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.filter = Some(Filter::Eq("kind".into(), FieldValue::Str("table".into())));
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.chunk_id, 1);
    }

    #[test]
    fn invalid_request_errs() {
        let e = engine();
        let mut r = req("x");
        r.top_k = 0;
        assert!(e.search(&r, None).is_err());
    }

    #[test]
    fn hybrid_falls_back_to_keyword_without_vector() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1))
            .unwrap();
        e.commit().unwrap();
        let mut r = req("alpha");
        r.mode = SearchMode::Hybrid; // 无 req.vector → 退化为全文
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vector, None);
    }

    #[test]
    fn real_hybrid_fuses_keyword_and_vector() {
        let mut e = engine();
        // c1 文本含 "alpha"，向量 [1,0]；c2 文本含 "beta"，向量 [0,1]
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();

        // hybrid：查询词 "alpha" + 查询向量偏向 [0,1]（语义偏 c2）
        let mut r = req("alpha");
        r.mode = SearchMode::Hybrid;
        r.vector = Some(vec![0.0, 1.0]);
        let hits = e.search(&r, None).unwrap();
        // 两路都召回：c1（keyword 命中）+ c2（vector 命中）
        assert_eq!(hits.len(), 2);
        let c1 = hits.iter().find(|h| h.id.chunk_id == 1).unwrap();
        let c2 = hits.iter().find(|h| h.id.chunk_id == 2).unwrap();
        assert!(c1.bm25.is_some()); // c1 有 keyword 分
        assert!(c2.vector.is_some()); // c2 有 vector 分
    }

    #[test]
    fn persist_load_roundtrip_keeps_vectors_and_lsn() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = TextIndexConfig::default();
        // 首启：空，lsn=0
        let (mut e, lsn0) = Engine::open(dir.path(), cfg).unwrap();
        assert_eq!(lsn0, Lsn(0));
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "beta", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.persist(dir.path(), Lsn(42)).unwrap();
        drop(e);

        // 重开：检查点续传 + 向量在（无需重嵌）
        let (e2, lsn) = Engine::open(dir.path(), TextIndexConfig::default()).unwrap();
        assert_eq!(lsn, Lsn(42));
        let mut r = req("");
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        let hits = e2.search(&r, None).unwrap();
        assert_eq!(hits[0].id.chunk_id, 1);
        assert!(hits[0].vector.is_some());
        // 文本也在（keyword 路）
        let kw = e2.search(&req("beta"), None).unwrap();
        assert_eq!(kw[0].id.chunk_id, 2);
    }

    /// 二值粗筛后端化：首启用 `BruteBinary` → 检查点记 `brute_binary` → 重开（即便默认 `Brute`）
    /// 仍恢复粗筛档（oversample 取默认，同 HNSW 参数策略）；检索可用。
    #[test]
    fn persist_reopen_restores_brute_binary_backend() {
        let dir = tempfile::tempdir().unwrap();
        let (mut e, _) = Engine::open_with(
            dir.path(),
            TextIndexConfig::default(),
            VectorBackendKind::BruteBinary(8),
        )
        .unwrap();
        assert_eq!(e.vector.kind_str(), "brute_binary");
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.persist(dir.path(), Lsn(7)).unwrap();
        drop(e);

        // 重开**用默认 Brute**：检查点记的 brute_binary 覆盖默认 → 仍粗筛档。
        let (e2, lsn) = Engine::open_with(
            dir.path(),
            TextIndexConfig::default(),
            VectorBackendKind::Brute,
        )
        .unwrap();
        assert_eq!(lsn, Lsn(7));
        assert_eq!(
            e2.vector.kind_str(),
            "brute_binary",
            "检查点应恢复粗筛档（覆盖默认 brute）"
        );
        let mut r = req("");
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        assert_eq!(e2.search(&r, None).unwrap()[0].id.chunk_id, 1);
    }

    /// 旋转粗筛后端化：首启 `BruteBinaryRotated` → 检查点记 `brute_binary_rotated` → 重开（默认
    /// `Brute`）仍恢复旋转档（load 重建旋转矩阵）；检索可用。
    #[test]
    fn persist_reopen_restores_brute_binary_rotated_backend() {
        let dir = tempfile::tempdir().unwrap();
        let (mut e, _) = Engine::open_with(
            dir.path(),
            TextIndexConfig::default(),
            VectorBackendKind::BruteBinaryRotated(8),
        )
        .unwrap();
        assert_eq!(e.vector.kind_str(), "brute_binary_rotated");
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "alpha", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.persist(dir.path(), Lsn(9)).unwrap();
        drop(e);

        let (e2, lsn) = Engine::open_with(
            dir.path(),
            TextIndexConfig::default(),
            VectorBackendKind::Brute,
        )
        .unwrap();
        assert_eq!(lsn, Lsn(9));
        assert_eq!(
            e2.vector.kind_str(),
            "brute_binary_rotated",
            "检查点应恢复旋转粗筛档（覆盖默认 brute）"
        );
        let mut r = req("");
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        assert_eq!(e2.search(&r, None).unwrap()[0].id.chunk_id, 1);
    }

    #[test]
    fn rerank_reorders_by_overlap() {
        use fastsearch_core::RerankSpec;
        let mut e = engine();
        // 两 chunk 都含 "apple"，但 chunk 2 与 query "apple banana" 词项更重叠
        e.ingest(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "apple cherry date", 1),
        )
        .unwrap();
        e.ingest(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "apple banana", 2),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("apple banana");
        r.rerank = Some(RerankSpec {
            model: "lexical".into(),
            top_k: 10,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 2);
        // chunk 2（与 query 完全重叠）应被 rerank 提前
        assert_eq!(hits[0].id.chunk_id, 2);
        assert!(hits[0].rerank.unwrap() > hits[1].rerank.unwrap());
    }

    #[test]
    fn rerank_uses_real_text_for_vector_only_hits() {
        // H1-A 回归：mode=Vector 命中不经 keyword 路 → text_map 无该条；rerank 必须回真源取
        // STORED 正文打分，否则拿空串 → 全 0 分、排序退化为 gid 序，向量排名被摧毁。
        use fastsearch_core::RerankSpec;
        use fastsearch_sync::IndexSink;
        let mut e = engine();
        e.set_embedder(Box::new(HashEmbedder::new(32)));
        let c = chunk("d.pdf", 1, ChunkKind::Paragraph, "alpha beta gamma", 1);
        e.apply_upsert("kb", &c).unwrap();
        e.commit().unwrap();

        // Vector 模式的查询向量来自 req.vector（server 层嵌入查询文本后注入）。用同款确定性
        // HashEmbedder 嵌同一文本 → 与索引里该 chunk 的向量 cos=1，命中它自身。
        let qv = HashEmbedder::new(32)
            .embed(&["alpha beta gamma".into()], EmbedKind::Query)
            .unwrap()
            .pop()
            .unwrap();
        let req = SearchRequest {
            query: "alpha beta gamma".into(),
            vector: Some(qv),
            mode: SearchMode::Vector,
            rerank: Some(RerankSpec {
                model: "lexical".into(),
                top_k: 10,
            }),
            top_k: 5,
            candidates: 10,
            ..Default::default()
        };
        let hits = e.search(&req, None).unwrap();
        let hit = hits
            .iter()
            .find(|h| h.id.chunk_id == 1)
            .expect("chunk 应被向量召回");
        assert_eq!(
            hit.rerank,
            Some(1.0),
            "rerank 应对真源正文完全重叠打 1.0，而非空串 0.0"
        );
    }

    #[test]
    fn rerank_top_k_caps_window() {
        // M5 回归：rerank.top_k 限住重排窗口（融合分最高的 N 个），其余候选丢弃。此前该值无处读取，
        // rerank 窗口=整个候选集。
        use fastsearch_core::RerankSpec;
        let mut e = engine();
        for id in 1..=5u64 {
            e.ingest(
                "kb",
                &chunk("a.pdf", id, ChunkKind::Paragraph, "data here", id as u32),
            )
            .unwrap();
        }
        e.commit().unwrap();
        let mut r = req("data");
        r.top_k = 10;
        r.rerank = Some(RerankSpec {
            model: "lexical".into(),
            top_k: 2,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "rerank 窗口应截到 rerank.top_k=2（而非全部 5 条）"
        );
    }

    #[test]
    fn collapse_by_section_id_scoped_per_doc() {
        // M6 回归：section_id 是 doc 内编号，collapse{field:section_id} 分组键必须含 collection+doc_id，
        // 否则不同文档的同号 section 互相挤占名额、静默丢命中。
        use fastsearch_core::Collapse;
        let mut e = engine();
        e.ingest("kb", &chunk_sec("a.pdf", 1, "data here", 1))
            .unwrap();
        e.ingest("kb", &chunk_sec("b.pdf", 2, "data here", 1))
            .unwrap(); // 不同 doc、同 section 号
        e.commit().unwrap();
        let mut r = req("data");
        r.collapse = Some(Collapse {
            field: "section_id".into(),
            max_per_group: 1,
        });
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits.len(), 2, "不同文档的同号 section 不应互相挤占名额");
    }

    #[test]
    fn image_query_empty_text_hybrid_equals_vector() {
        // M7 回归：mode=Hybrid（serde 默认）+ query_image + 空 query 时，keyword 路退化成 match-all
        // 污染融合。修复后空-query 图像检索跳过 keyword，Hybrid 结果应与显式 mode=Vector 完全一致。
        use fastsearch_sync::IndexSink;
        let mut e = engine();
        e.set_embedder(Box::new(CrossModalHashEmbedder::new(32)));
        let mk = |id: u64, bytes: Vec<u8>| {
            let mut c = chunk("d.pdf", id, ChunkKind::Image, "", id as u32);
            c.media_bytes = Some(bytes);
            c
        };
        let a = vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3, 4, 5];
        let bytes = [
            (1u64, a.clone()),
            (2, vec![0xFF, 0xD8, 9, 8, 7, 6, 5]),
            (3, vec![0x47, 0x49, 0x46, 3, 1, 4, 1, 5]),
            (4, vec![0x42, 0x4D, 2, 7, 1, 8, 2, 8]),
            (5, vec![0x00, 0x01, 6, 1, 8, 0, 3, 3]),
            (6, vec![0x7F, 0x45, 4, 6, 6, 9, 2, 0]),
        ];
        for (id, b) in bytes {
            e.apply_upsert("kb", &mk(id, b)).unwrap();
        }
        e.commit().unwrap();
        let ids = |mode| -> Vec<u64> {
            e.search(
                &SearchRequest {
                    mode,
                    query_image: Some(a.clone()),
                    top_k: 3,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .iter()
            .map(|h| h.id.chunk_id)
            .collect()
        };
        let hybrid = ids(SearchMode::Hybrid);
        let vector = ids(SearchMode::Vector);
        assert_eq!(
            hybrid, vector,
            "空-query 图像检索的 Hybrid 应与 Vector 一致（无 match-all 污染）"
        );
        assert_eq!(hybrid.first(), Some(&1), "查询图=a → 命中 a 自身");
    }

    #[test]
    fn facets_count_over_results() {
        let mut e = engine();
        e.ingest("kb", &chunk("a.pdf", 1, ChunkKind::Table, "data here", 1))
            .unwrap();
        e.ingest("kb", &chunk("a.pdf", 2, ChunkKind::Table, "data here", 2))
            .unwrap();
        e.ingest(
            "kb",
            &chunk("b.pdf", 3, ChunkKind::Paragraph, "data here", 1),
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req("data");
        r.facets = vec!["kind".into(), "doc_id".into()];
        let (hits, facets) = e.search_with_facets(&r, None).unwrap();
        assert_eq!(hits.len(), 3);
        // kind: table=2, paragraph=1（降序）
        let kind = &facets["kind"];
        assert_eq!(kind[0], ("table".into(), 2));
        assert_eq!(kind[1], ("paragraph".into(), 1));
        // doc_id: a.pdf=2, b.pdf=1
        let doc = &facets["doc_id"];
        assert_eq!(doc[0], ("a.pdf".into(), 2));
        assert_eq!(doc[1], ("b.pdf".into(), 1));
    }

    #[test]
    fn vector_only_mode() {
        let mut e = engine();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 1, ChunkKind::Paragraph, "x", 1),
            vec![1.0, 0.0],
        )
        .unwrap();
        e.ingest_vector(
            "kb",
            &chunk("a.pdf", 2, ChunkKind::Paragraph, "y", 2),
            vec![0.0, 1.0],
        )
        .unwrap();
        e.commit().unwrap();
        let mut r = req(""); // 纯向量，无关键词
        r.mode = SearchMode::Vector;
        r.vector = Some(vec![1.0, 0.0]);
        let hits = e.search(&r, None).unwrap();
        assert_eq!(hits[0].id.chunk_id, 1); // 最近向量
        assert!(hits[0].vector.is_some());
        assert!(hits[0].bm25.is_none());
    }
}
