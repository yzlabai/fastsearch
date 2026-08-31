//! Independent document-ingestion worker.
//!
//! PostgreSQL is used only for fenced job claims. All state changes and chunk publication go
//! through the server's worker endpoints, so identity reconstruction and index publication retain
//! one authoritative implementation.

use anyhow::{anyhow, bail, Context, Result};
use fastsearch_core::{BBox, Chunk, ChunkKind, ImageVectorStatus, MediaRef, Metadata};
use fastsearch_engine::{
    LocalObjectStore, ObjectError, ObjectErrorKind, ObjectStore, S3ObjectStore,
};
use fastsearch_ingest_adapter::{
    chunks_for_file, ChunkProfile, Enhancements, ImageBytes, ParseOptions,
};
use fastsearch_pg::{retry_backoff_ms, JobLease, JobStore};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_LEASE_MS: u64 = 60_000;
const DEFAULT_HEARTBEAT_MS: u64 = 20_000;
const DEFAULT_IDLE_MIN_MS: u64 = 500;
const DEFAULT_IDLE_MAX_MS: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub database_url: String,
    pub jobs_table: String,
    pub chunks_table: String,
    pub server: String,
    pub worker_key: String,
    pub concurrency: usize,
    pub lease_ms: u64,
    pub heartbeat_ms: u64,
    pub idle_min_ms: u64,
    pub idle_max_ms: u64,
    pub max_document_bytes: usize,
    pub http_timeout: Duration,
}

impl WorkerConfig {
    pub fn from_env() -> Result<Self> {
        let database_url = required_env("DATABASE_URL")?;
        let worker_key = required_env("FASTSEARCH_WORKER_KEY")?;
        let concurrency = env_usize("FASTSEARCH_WORKER_CONCURRENCY", 1)?;
        if !(1..=64).contains(&concurrency) {
            bail!("FASTSEARCH_WORKER_CONCURRENCY must be in 1..=64");
        }
        let lease_ms = env_u64("FASTSEARCH_WORKER_LEASE_MS", DEFAULT_LEASE_MS)?;
        let heartbeat_ms = env_u64("FASTSEARCH_WORKER_HEARTBEAT_MS", DEFAULT_HEARTBEAT_MS)?;
        if heartbeat_ms == 0 || lease_ms < 1_000 || heartbeat_ms >= lease_ms {
            bail!("worker lease must be >=1000ms and heartbeat must be >0 and smaller than lease");
        }
        let idle_min_ms = env_u64("FASTSEARCH_WORKER_IDLE_MIN_MS", DEFAULT_IDLE_MIN_MS)?;
        let idle_max_ms = env_u64("FASTSEARCH_WORKER_IDLE_MAX_MS", DEFAULT_IDLE_MAX_MS)?;
        if idle_min_ms == 0 || idle_min_ms > idle_max_ms {
            bail!("worker idle backoff requires 0 < min <= max");
        }
        let max_document_bytes = std::env::var("FASTSEARCH_MAX_DOCUMENT_BYTES")
            .or_else(|_| std::env::var("FASTSEARCH_S3_MAX_IMAGE_BYTES"))
            .ok()
            .map(|value| {
                value.parse::<usize>().with_context(|| {
                    format!("FASTSEARCH_MAX_DOCUMENT_BYTES={value:?} is not an integer")
                })
            })
            .transpose()?
            .unwrap_or(DEFAULT_MAX_DOCUMENT_BYTES)
            .max(1);
        let timeout_secs = env_u64("FASTSEARCH_TIMEOUT_SECS", 30)?.max(1);
        Ok(Self {
            database_url,
            jobs_table: std::env::var("FASTSEARCH_INGEST_JOBS_TABLE")
                .unwrap_or_else(|_| "fastsearch_ingest_jobs".into()),
            chunks_table: std::env::var("FASTSEARCH_PG_TABLE")
                .unwrap_or_else(|_| "fastsearch_chunks".into()),
            server: std::env::var("FASTSEARCH_SERVER")
                .unwrap_or_else(|_| "http://localhost:8642".into())
                .trim_end_matches('/')
                .to_string(),
            worker_key,
            concurrency,
            lease_ms,
            heartbeat_ms,
            idle_min_ms,
            idle_max_ms,
            max_document_bytes,
            http_timeout: Duration::from_secs(timeout_secs),
        })
    }
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{key} is required"))
}

fn env_u64(key: &str, default: u64) -> Result<u64> {
    std::env::var(key)
        .ok()
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("{key}={value:?} is not an integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn env_usize(key: &str, default: usize) -> Result<usize> {
    std::env::var(key)
        .ok()
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("{key}={value:?} is not an integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[derive(Clone)]
struct WorkerClient {
    base: String,
    key: String,
    agent: ureq::Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HttpError {
    Status(u16, String),
    Transport(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(code, body) => write!(f, "server returned {code}: {body}"),
            Self::Transport(message) => write!(f, "worker HTTP transport failed: {message}"),
        }
    }
}

impl WorkerClient {
    fn new(config: &WorkerConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(config.http_timeout)
            .timeout_read(config.http_timeout)
            .timeout_write(config.http_timeout)
            .build();
        Self {
            base: config.server.clone(),
            key: config.worker_key.clone(),
            agent,
        }
    }

    fn post(&self, path: &str, body: Value) -> std::result::Result<Value, HttpError> {
        let url = format!("{}{path}", self.base);
        match self
            .agent
            .post(&url)
            .set("authorization", &format!("Bearer {}", self.key))
            .send_json(body)
        {
            Ok(response) => response
                .into_json()
                .map_err(|error| HttpError::Transport(format!("invalid JSON response: {error}"))),
            Err(ureq::Error::Status(code, response)) => Err(HttpError::Status(
                code,
                response.into_string().unwrap_or_default(),
            )),
            Err(error) => Err(HttpError::Transport(error.to_string())),
        }
    }

    async fn status(
        &self,
        lease: &JobLease,
        state: &str,
        detail: Value,
        lease_ms: u64,
    ) -> std::result::Result<Value, HttpError> {
        let client = self.clone();
        let path = format!("/v1/jobs/{}/status", lease.job.job_id);
        let body = json!({
            "lease_job_id": lease.job.job_id,
            "lease_owner": lease.owner,
            "lease_epoch": lease.epoch,
            "state": state,
            "stage_detail": detail,
            "lease_for_ms": lease_ms,
        });
        tokio::task::spawn_blocking(move || client.post(&path, body))
            .await
            .map_err(|error| HttpError::Transport(format!("HTTP task failed: {error}")))?
    }

    async fn fail(
        &self,
        lease: &JobLease,
        stage: &str,
        message: &str,
        next_attempt_at_ms: i64,
        retryable: bool,
    ) -> std::result::Result<Value, HttpError> {
        let client = self.clone();
        let path = format!("/v1/jobs/{}/status", lease.job.job_id);
        let body = json!({
            "lease_job_id": lease.job.job_id,
            "lease_owner": lease.owner,
            "lease_epoch": lease.epoch,
            "state": "failed",
            "error": truncate_error(message),
            "error_stage": stage,
            "next_attempt_at_ms": next_attempt_at_ms,
            "retryable": retryable,
        });
        tokio::task::spawn_blocking(move || client.post(&path, body))
            .await
            .map_err(|error| HttpError::Transport(format!("HTTP task failed: {error}")))?
    }

    async fn publish(
        &self,
        lease: &JobLease,
        chunks: Vec<WorkerChunk>,
        store_media: WorkerStoreMedia,
    ) -> std::result::Result<Value, HttpError> {
        let client = self.clone();
        let path = format!("/v1/jobs/{}/chunks", lease.job.job_id);
        let body = json!({
            "lease_job_id": lease.job.job_id,
            "lease_owner": lease.owner,
            "lease_epoch": lease.epoch,
            "store_media": store_media,
            "chunks": chunks,
        });
        tokio::task::spawn_blocking(move || client.post(&path, body))
            .await
            .map_err(|error| HttpError::Transport(format!("HTTP task failed: {error}")))?
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WorkerStoreMedia {
    Inline,
    Object,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerChunk {
    chunk_id: u64,
    kind: ChunkKind,
    text: String,
    page: u32,
    bbox: BBox,
    heading_path: Vec<String>,
    section_id: u64,
    char_len: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    media: Option<MediaRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    media_bytes: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_vector_status: Option<ImageVectorStatus>,
    metadata: Metadata,
    searchable: bool,
}

impl From<Chunk> for WorkerChunk {
    fn from(chunk: Chunk) -> Self {
        Self {
            chunk_id: chunk.chunk_id,
            kind: chunk.kind,
            text: chunk.text,
            page: chunk.page,
            bbox: chunk.bbox,
            heading_path: chunk.heading_path,
            section_id: chunk.section_id,
            char_len: chunk.char_len,
            media: chunk.media,
            media_bytes: chunk.media_bytes,
            image_vector_status: chunk.image_vector_status,
            metadata: chunk.metadata,
            searchable: chunk.searchable,
        }
    }
}

#[derive(Debug, Clone)]
struct ParseSettings {
    chunk_profile: ChunkProfile,
    images: ImageBytes,
    enhancements: Enhancements,
}

fn parse_settings(profile: &Value) -> Result<ParseSettings> {
    let object = profile
        .as_object()
        .ok_or_else(|| anyhow!("parse_profile must be a JSON object"))?;
    let chunking = object
        .get("chunking")
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| anyhow!("parse_profile.chunking must be an object"))
        })
        .transpose()?
        .unwrap_or(object);
    let name = optional_str(chunking, "name")?.unwrap_or("docparse");
    let version = optional_u64(chunking, "version")?.unwrap_or(1);
    let target = optional_u64(chunking, "target_chars")?.unwrap_or(800);
    let overlap = optional_u64(chunking, "overlap_chars")?.unwrap_or(0);
    let table_markdown = optional_bool(chunking, "table_markdown")?.unwrap_or(false);
    let version = u32::try_from(version).context("chunk profile version is too large")?;
    let target = usize::try_from(target).context("chunk target is too large")?;
    let overlap = usize::try_from(overlap).context("chunk overlap is too large")?;
    let images = match optional_str(object, "images")?.unwrap_or("object") {
        "object" => ImageBytes::Object,
        "inline" => ImageBytes::Inline,
        "none" => ImageBytes::None,
        other => bail!("parse_profile.images must be object, inline or none; got {other:?}"),
    };
    let enhancements = Enhancements {
        ocr: optional_bool(object, "ocr")?.unwrap_or(false),
        tables: optional_bool(object, "tables")?.unwrap_or(false),
        vlm: optional_bool(object, "vlm")?.unwrap_or(false),
    };
    ensure_requested_features(enhancements)?;
    Ok(ParseSettings {
        chunk_profile: ChunkProfile::new(name, version, target, overlap, table_markdown)?,
        images,
        enhancements,
    })
}

fn optional_str<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    map.get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("parse_profile.{key} must be a string"))
        })
        .transpose()
}

fn optional_u64(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>> {
    map.get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| anyhow!("parse_profile.{key} must be a non-negative integer"))
        })
        .transpose()
}

fn optional_bool(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>> {
    map.get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| anyhow!("parse_profile.{key} must be a boolean"))
        })
        .transpose()
}

fn ensure_requested_features(enhancements: Enhancements) -> Result<()> {
    if enhancements.ocr && !cfg!(feature = "parse-ocr") {
        bail!("parse_profile requests OCR but worker was not built with feature parse-ocr");
    }
    if enhancements.tables && !cfg!(feature = "parse-tables") {
        bail!("parse_profile requests tables but worker was not built with feature parse-tables");
    }
    if enhancements.vlm && !cfg!(feature = "parse-vlm") {
        bail!("parse_profile requests VLM but worker was not built with feature parse-vlm");
    }
    Ok(())
}

fn object_store_from_env(max_bytes: usize) -> Result<Arc<dyn ObjectStore>> {
    if let Ok(endpoint) = std::env::var("FASTSEARCH_S3_ENDPOINT") {
        let region = std::env::var("FASTSEARCH_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let bucket = required_env("FASTSEARCH_S3_BUCKET")?;
        let access_key = required_env("FASTSEARCH_S3_ACCESS_KEY")?;
        let secret_key = required_env("FASTSEARCH_S3_SECRET_KEY")?;
        return Ok(Arc::new(
            S3ObjectStore::new(endpoint, region, bucket, access_key, secret_key)
                .with_max_bytes(max_bytes),
        ));
    }
    let root = required_env("FASTSEARCH_OBJECT_DIR")?;
    let bucket =
        std::env::var("FASTSEARCH_OBJECT_BUCKET").unwrap_or_else(|_| "fastsearch-assets".into());
    Ok(Arc::new(
        LocalObjectStore::new(root, bucket).with_max_bytes(max_bytes),
    ))
}

pub async fn run_from_env() -> Result<()> {
    let config = WorkerConfig::from_env()?;
    let objects = object_store_from_env(config.max_document_bytes)?;
    run(config, objects).await
}

pub async fn run(config: WorkerConfig, objects: Arc<dyn ObjectStore>) -> Result<()> {
    let mut slots = tokio::task::JoinSet::new();
    let run_id = worker_run_id();
    for slot in 0..config.concurrency {
        let config = config.clone();
        let objects = Arc::clone(&objects);
        let owner = format!("{run_id}-{slot}");
        slots.spawn(async move { run_slot(config, objects, owner).await });
    }
    match slots.join_next().await {
        Some(Ok(Err(error))) => Err(error),
        Some(Err(error)) => Err(anyhow!("worker slot panicked: {error}")),
        Some(Ok(Ok(()))) => Err(anyhow!("worker slot exited unexpectedly")),
        None => Err(anyhow!("worker started with no slots")),
    }
}

async fn run_slot(
    config: WorkerConfig,
    objects: Arc<dyn ObjectStore>,
    owner: String,
) -> Result<()> {
    let client = WorkerClient::new(&config);
    let mut idle_ms = config.idle_min_ms;
    loop {
        let jobs = match JobStore::connect_with_chunks_table(
            &config.database_url,
            config.jobs_table.clone(),
            config.chunks_table.clone(),
        )
        .await
        {
            Ok(jobs) => jobs,
            Err(error) => {
                eprintln!("warn: ingest worker PG connect failed; retrying: {error}");
                tokio::time::sleep(Duration::from_millis(idle_ms)).await;
                idle_ms = idle_ms.saturating_mul(2).min(config.idle_max_ms);
                continue;
            }
        };
        if let Err(error) = jobs.ensure_schema().await {
            eprintln!("warn: ingest worker PG schema check failed; retrying: {error}");
            tokio::time::sleep(Duration::from_millis(idle_ms)).await;
            idle_ms = idle_ms.saturating_mul(2).min(config.idle_max_ms);
            continue;
        }
        idle_ms = config.idle_min_ms;
        loop {
            let mut claimed = match jobs.claim(&owner, 1, config.lease_ms).await {
                Ok(claimed) => claimed,
                Err(error) => {
                    eprintln!("warn: ingest worker PG claim failed; reconnecting: {error}");
                    break;
                }
            };
            let Some(lease) = claimed.pop() else {
                tokio::time::sleep(Duration::from_millis(idle_ms)).await;
                idle_ms = idle_ms.saturating_mul(2).min(config.idle_max_ms);
                continue;
            };
            idle_ms = config.idle_min_ms;
            match process_lease(&config, &client, &objects, &lease).await {
                Ok(WorkOutcome::Indexed) => eprintln!(
                    "ingest worker indexed job={} doc={}/{}",
                    lease.job.job_id, lease.job.collection, lease.job.doc_id
                ),
                Ok(WorkOutcome::Failed) => eprintln!(
                    "ingest worker recorded failure job={} retry={}/{}",
                    lease.job.job_id,
                    lease.job.retry_count.saturating_add(1),
                    lease.job.max_retries
                ),
                Ok(WorkOutcome::LeaseLost) => eprintln!(
                    "ingest worker stopped stale work job={} lease_epoch={}",
                    lease.job.job_id, lease.epoch
                ),
                Err(error) => return Err(error),
            }
        }
        tokio::time::sleep(Duration::from_millis(idle_ms)).await;
        idle_ms = idle_ms.saturating_mul(2).min(config.idle_max_ms);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkOutcome {
    Indexed,
    Failed,
    LeaseLost,
}

#[derive(Debug)]
enum WorkError {
    Retryable {
        stage: &'static str,
        message: String,
    },
    Terminal {
        stage: &'static str,
        message: String,
    },
    LeaseLost,
    Fatal(String),
}

impl WorkError {
    fn retryable(stage: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Retryable {
            stage,
            message: error.to_string(),
        }
    }

    fn terminal(stage: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Terminal {
            stage,
            message: error.to_string(),
        }
    }
}

async fn process_lease(
    config: &WorkerConfig,
    client: &WorkerClient,
    objects: &Arc<dyn ObjectStore>,
    lease: &JobLease,
) -> Result<WorkOutcome> {
    let heartbeat = HeartbeatGuard::start(client.clone(), lease.clone(), config);
    let result = process_inner(config, client, objects, lease, &heartbeat).await;
    let heartbeat_failure = heartbeat.stop().await;
    if let Some(error) = heartbeat_failure {
        if is_auth_error(&error) {
            return Err(anyhow!(
                "worker API credential lacks the worker capability during heartbeat: {error}"
            ));
        }
        return Ok(WorkOutcome::LeaseLost);
    }
    match result {
        Ok(()) => Ok(WorkOutcome::Indexed),
        Err(WorkError::LeaseLost) => Ok(WorkOutcome::LeaseLost),
        Err(WorkError::Fatal(message)) => Err(anyhow!(message)),
        Err(WorkError::Retryable { stage, message }) => {
            report_work_failure(client, lease, stage, &message, true).await
        }
        Err(WorkError::Terminal { stage, message }) => {
            report_work_failure(client, lease, stage, &message, false).await
        }
    }
}

async fn report_work_failure(
    client: &WorkerClient,
    lease: &JobLease,
    stage: &'static str,
    message: &str,
    retryable: bool,
) -> Result<WorkOutcome> {
    let next = next_attempt_at_ms(lease);
    match client.fail(lease, stage, message, next, retryable).await {
        Ok(_) => Ok(WorkOutcome::Failed),
        Err(error) if is_fence_error(&error) => Ok(WorkOutcome::LeaseLost),
        Err(error) if is_auth_error(&error) => Err(anyhow!(
            "worker API credential lacks the worker capability while reporting failure: {error}"
        )),
        Err(error) => {
            eprintln!(
                "warn: unable to report job={} failure; lease expiry will recover it: {error}",
                lease.job.job_id
            );
            Ok(WorkOutcome::Failed)
        }
    }
}

async fn process_inner(
    config: &WorkerConfig,
    client: &WorkerClient,
    objects: &Arc<dyn ObjectStore>,
    lease: &JobLease,
    heartbeat: &HeartbeatGuard,
) -> std::result::Result<(), WorkError> {
    let source_uri = lease.job.source_uri.clone();
    let tenant = lease.job.tenant.clone();
    let max_bytes = config.max_document_bytes;
    let objects = Arc::clone(objects);
    let bytes = tokio::task::spawn_blocking(move || {
        objects
            .validate_ref(&source_uri, tenant.as_deref())
            .and_then(|_| objects.get(&source_uri, max_bytes))
    })
    .await
    .map_err(|error| WorkError::retryable("fetch", error))?
    .map_err(|error| classify_object_error("fetch", error))?;
    heartbeat.check()?;

    let settings = parse_settings(&lease.job.parse_profile)
        .map_err(|error| classify_deterministic_error("profile", error))?;
    let doc_id = lease.job.doc_id.clone();
    let filename = lease.job.filename.clone();
    let media_type = lease.job.media_type.clone();
    let parsed = tokio::task::spawn_blocking(move || {
        parse_downloaded_document(
            bytes.bytes,
            filename.as_deref(),
            media_type.as_deref(),
            &doc_id,
            settings,
        )
    })
    .await
    .map_err(|error| WorkError::retryable("parsing", error))?
    .map_err(|error| classify_deterministic_error("parsing", error))?;
    heartbeat.check()?;

    let chunks_len = parsed.chunks.len();
    client
        .status(
            lease,
            "chunking",
            json!({"chunk_count": chunks_len}),
            config.lease_ms,
        )
        .await
        .map_err(|error| map_http_error("chunking", error))?;
    heartbeat.check()?;
    client
        .publish(lease, parsed.chunks, parsed.store_media)
        .await
        .map_err(|error| map_http_error("publish", error))?;
    Ok(())
}

struct ParsedDocument {
    chunks: Vec<WorkerChunk>,
    store_media: WorkerStoreMedia,
}

fn parse_downloaded_document(
    bytes: Vec<u8>,
    filename: Option<&str>,
    media_type: Option<&str>,
    doc_id: &str,
    settings: ParseSettings,
) -> Result<ParsedDocument> {
    let suffix = temporary_suffix(filename, media_type);
    let mut file = tempfile::Builder::new()
        .prefix("fastsearch-ingest-")
        .suffix(&suffix)
        .tempfile()
        .context("create temporary source file")?;
    file.write_all(&bytes)
        .context("write temporary source file")?;
    file.flush().context("flush temporary source file")?;
    let store_media = match settings.images {
        ImageBytes::Inline => WorkerStoreMedia::Inline,
        ImageBytes::Object | ImageBytes::None => WorkerStoreMedia::Object,
    };
    let chunks = chunks_for_file(&ParseOptions {
        file: file.path().to_path_buf(),
        doc_id: doc_id.to_string(),
        // These fields are discarded by WorkerChunk. Server reconstructs both from the job row.
        tenant: None,
        acl: Vec::new(),
        images: settings.images,
        chunk_profile: settings.chunk_profile,
        enhancements: settings.enhancements,
    })?
    .into_iter()
    .map(WorkerChunk::from)
    .collect::<Vec<_>>();
    if chunks.is_empty() {
        bail!("parser produced no chunks");
    }
    Ok(ParsedDocument {
        chunks,
        store_media,
    })
}

fn temporary_suffix(filename: Option<&str>, media_type: Option<&str>) -> String {
    let extension = filename
        .and_then(|name| Path::new(name).extension())
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 12
                && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| extension_for_media_type(media_type).to_string());
    format!(".{extension}")
}

fn extension_for_media_type(media_type: Option<&str>) -> &'static str {
    match media_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
    {
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/plain" => "txt",
        "message/rfc822" => "eml",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "bin",
    }
}

fn map_http_error(stage: &'static str, error: HttpError) -> WorkError {
    if is_fence_error(&error) {
        WorkError::LeaseLost
    } else if is_auth_error(&error) {
        WorkError::Fatal(error.to_string())
    } else if matches!(error, HttpError::Status(400..=499, _))
        && !matches!(error, HttpError::Status(408 | 425 | 429, _))
    {
        WorkError::terminal(stage, error)
    } else {
        WorkError::retryable(stage, error)
    }
}

fn classify_object_error(stage: &'static str, error: ObjectError) -> WorkError {
    match error.kind {
        ObjectErrorKind::NotFound | ObjectErrorKind::Transient => {
            WorkError::retryable(stage, error)
        }
        ObjectErrorKind::Forbidden
        | ObjectErrorKind::InvalidMetadata
        | ObjectErrorKind::TooLarge
        | ObjectErrorKind::UnsupportedMediaType => WorkError::terminal(stage, error),
    }
}

fn classify_deterministic_error(stage: &'static str, error: impl std::fmt::Display) -> WorkError {
    WorkError::terminal(stage, error)
}

fn is_fence_error(error: &HttpError) -> bool {
    matches!(error, HttpError::Status(404 | 409, _))
}

fn is_auth_error(error: &HttpError) -> bool {
    matches!(error, HttpError::Status(401 | 403, _))
}

struct HeartbeatGuard {
    stop: tokio::sync::watch::Sender<bool>,
    lost: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<HttpError>>>,
    task: tokio::task::JoinHandle<()>,
}

impl HeartbeatGuard {
    fn start(client: WorkerClient, lease: JobLease, config: &WorkerConfig) -> Self {
        let (stop, mut stop_rx) = tokio::sync::watch::channel(false);
        let lost = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let task_lost = Arc::clone(&lost);
        let task_failure = Arc::clone(&failure);
        let heartbeat_ms = config.heartbeat_ms;
        let lease_ms = config.lease_ms;
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(heartbeat_ms)) => {
                        if let Err(error) = client.status(&lease, "heartbeat", json!({}), lease_ms).await {
                            task_lost.store(true, Ordering::Release);
                            *task_failure.lock().expect("heartbeat failure mutex") = Some(error);
                            break;
                        }
                    }
                    changed = stop_rx.changed() => {
                        if changed.is_err() || *stop_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            stop,
            lost,
            failure,
            task,
        }
    }

    fn check(&self) -> std::result::Result<(), WorkError> {
        if self.lost.load(Ordering::Acquire) {
            Err(WorkError::LeaseLost)
        } else {
            Ok(())
        }
    }

    async fn stop(self) -> Option<HttpError> {
        let _ = self.stop.send(true);
        let _ = self.task.await;
        self.failure
            .lock()
            .expect("heartbeat failure mutex")
            .clone()
    }
}

fn truncate_error(message: &str) -> String {
    message.chars().take(2_000).collect()
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn next_attempt_at_ms(lease: &JobLease) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let retry_count = u32::try_from(lease.job.retry_count).unwrap_or_default();
    let delay = retry_backoff_ms(retry_count, stable_hash(&lease.job.job_id));
    i64::try_from(now.saturating_add(u128::from(delay))).unwrap_or(i64::MAX)
}

fn worker_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("worker-{}-{now:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_pg::{IngestJob, IngestState};
    use std::sync::mpsc;

    #[test]
    fn worker_wire_chunk_cannot_carry_identity_or_document_coordinates() {
        let chunk = Chunk {
            doc_id: "secret-doc".into(),
            chunk_id: 7,
            kind: ChunkKind::Paragraph,
            text: "hello".into(),
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec![],
            section_id: 0,
            char_len: 5,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: Some("secret-tenant".into()),
            acl: vec!["admin".into()],
            metadata: Metadata::new(),
            searchable: true,
        };
        let value = serde_json::to_value(WorkerChunk::from(chunk)).unwrap();
        let keys = value.as_object().unwrap();
        for forbidden in ["collection", "doc_id", "tenant", "acl"] {
            assert!(!keys.contains_key(forbidden), "wire leaked {forbidden}");
        }
    }

    #[test]
    fn parse_profile_controls_chunking_and_rejects_malformed_values() {
        let settings = parse_settings(&json!({
            "chunking": {"name":"reports", "version":2, "target_chars":512,
                         "overlap_chars":64, "table_markdown":true},
            "images":"none"
        }))
        .unwrap();
        assert_eq!(settings.chunk_profile.target_chars(), 512);
        assert_eq!(settings.chunk_profile.overlap_chars(), 64);
        assert_eq!(settings.images, ImageBytes::None);
        assert!(parse_settings(&json!({"target_chars":"large"})).is_err());
        assert!(parse_settings(&json!({"images":"somewhere"})).is_err());
    }

    #[test]
    fn unsupported_heavy_profile_fails_loudly_in_light_worker() {
        if !cfg!(feature = "parse-ocr") {
            let error = parse_settings(&json!({"ocr":true}))
                .unwrap_err()
                .to_string();
            assert!(error.contains("parse-ocr"));
        }
    }

    #[test]
    fn temporary_file_preserves_safe_dispatch_suffix() {
        assert_eq!(temporary_suffix(Some("../../report.PDF"), None), ".pdf");
        assert_eq!(
            temporary_suffix(Some("bad.long-extension!"), Some("text/markdown")),
            ".md"
        );
        assert_eq!(
            temporary_suffix(None, Some("application/pdf; charset=binary")),
            ".pdf"
        );
    }

    #[test]
    fn retry_timestamp_moves_forward_deterministically() {
        let lease = sample_lease();
        let a = retry_backoff_ms(0, stable_hash(&lease.job.job_id));
        let b = retry_backoff_ms(0, stable_hash(&lease.job.job_id));
        assert_eq!(a, b);
        assert!((750..=1_250).contains(&a));
    }

    #[test]
    fn worker_failure_matrix_classifies_http_and_deterministic_errors() {
        for error in [
            HttpError::Status(408, "timeout".into()),
            HttpError::Status(425, "too early".into()),
            HttpError::Status(429, "rate limited".into()),
            HttpError::Status(503, "unavailable".into()),
            HttpError::Transport("connection reset".into()),
        ] {
            assert!(matches!(
                map_http_error("publish", error),
                WorkError::Retryable { .. }
            ));
        }
        for code in [400, 405, 413, 415, 422] {
            assert!(matches!(
                map_http_error("publish", HttpError::Status(code, "bad request".into())),
                WorkError::Terminal { .. }
            ));
        }
        for code in [404, 409] {
            assert!(matches!(
                map_http_error("publish", HttpError::Status(code, "stale".into())),
                WorkError::LeaseLost
            ));
        }
        for code in [401, 403] {
            assert!(matches!(
                map_http_error("publish", HttpError::Status(code, "denied".into())),
                WorkError::Fatal(_)
            ));
        }
        assert!(matches!(
            classify_deterministic_error("profile", anyhow!("invalid profile")),
            WorkError::Terminal { .. }
        ));
    }

    fn sample_lease() -> JobLease {
        JobLease {
            owner: "worker".into(),
            epoch: 3,
            job: IngestJob {
                job_id: "job-1".into(),
                collection: "kb".into(),
                doc_id: "d.md".into(),
                tenant: Some("acme".into()),
                acl: vec!["team".into()],
                source_uri: "s3://objects/acme/kb/d.md".into(),
                source_ready: true,
                cleanup_source_uri: None,
                content_sha256: "0".repeat(64),
                content_bytes: 1,
                media_type: Some("text/markdown".into()),
                filename: Some("d.md".into()),
                parse_profile: json!({}),
                state: IngestState::Parsing,
                stage_detail: json!({}),
                chunk_count: 0,
                lease_owner: Some("worker".into()),
                lease_epoch: 3,
                lease_until_ms: None,
                heartbeat_at_ms: None,
                retry_count: 0,
                max_retries: 3,
                next_attempt_at_ms: 0,
                error: None,
                error_stage: None,
                error_retryable: None,
                created_at_ms: 0,
                updated_at_ms: 0,
                started_at_ms: None,
                finished_at_ms: None,
            },
        }
    }

    fn test_config(server: String) -> WorkerConfig {
        WorkerConfig {
            database_url: "postgres://unused".into(),
            jobs_table: "jobs".into(),
            chunks_table: "chunks".into(),
            server,
            worker_key: "worker-key".into(),
            concurrency: 1,
            lease_ms: 60_000,
            heartbeat_ms: 20_000,
            idle_min_ms: 1,
            idle_max_ms: 2,
            max_document_bytes: 1024 * 1024,
            http_timeout: Duration::from_secs(2),
        }
    }

    fn spawn_worker_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, mpsc::Receiver<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for (mut stream, (status, body)) in
                listener.incoming().filter_map(Result::ok).zip(responses)
            {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                        Err(_) => break,
                    }
                    if let Some(split) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..split]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() - split - 4 >= length {
                            break;
                        }
                    }
                }
                tx.send(request).unwrap();
                let reason = if status == 200 { "OK" } else { "Conflict" };
                write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        (url, rx)
    }

    #[tokio::test]
    async fn downloaded_document_reaches_job_scoped_publish_without_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalObjectStore::new(directory.path(), "objects");
        let object = store
            .put(
                "acme/kb/note.md",
                b"# Revenue\nGross margin rose to 42%.\n",
                "text/markdown",
            )
            .unwrap();
        let (server, requests) = spawn_worker_server(vec![
            (200, r#"{"state":"chunking"}"#),
            (200, r#"{"state":"indexed","chunk_count":2}"#),
        ]);
        let mut lease = sample_lease();
        lease.job.source_uri = object.uri;
        let config = test_config(server);
        let client = WorkerClient::new(&config);
        let objects: Arc<dyn ObjectStore> = Arc::new(store);
        let outcome = process_lease(&config, &client, &objects, &lease)
            .await
            .unwrap();
        assert_eq!(outcome, WorkOutcome::Indexed);

        let status = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(status.starts_with("POST /v1/jobs/job-1/status"));
        let status_body: Value =
            serde_json::from_str(status.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap();
        assert_eq!(status_body["state"], "chunking");

        let publish = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(publish.starts_with("POST /v1/jobs/job-1/chunks"));
        let publish_body: Value =
            serde_json::from_str(publish.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap();
        assert_eq!(publish_body["store_media"], "object");
        assert!(!publish_body["chunks"].as_array().unwrap().is_empty());
        let encoded = publish_body.to_string();
        for forbidden in ["\"collection\"", "\"doc_id\"", "\"tenant\"", "\"acl\""] {
            assert!(!encoded.contains(forbidden), "publish leaked {forbidden}");
        }
    }

    #[tokio::test]
    async fn stale_lease_stops_before_chunk_publication() {
        let directory = tempfile::tempdir().unwrap();
        let store = LocalObjectStore::new(directory.path(), "objects");
        let object = store
            .put("acme/kb/note.md", b"# Note\ntext\n", "text/markdown")
            .unwrap();
        let (server, requests) =
            spawn_worker_server(vec![(409, r#"{"error":"worker lease expired"}"#)]);
        let mut lease = sample_lease();
        lease.job.source_uri = object.uri;
        let config = test_config(server);
        let client = WorkerClient::new(&config);
        let objects: Arc<dyn ObjectStore> = Arc::new(store);
        let outcome = process_lease(&config, &client, &objects, &lease)
            .await
            .unwrap();
        assert_eq!(outcome, WorkOutcome::LeaseLost);
        let request = String::from_utf8(requests.recv().unwrap()).unwrap();
        assert!(request.starts_with("POST /v1/jobs/job-1/status"));
        assert!(
            requests.try_recv().is_err(),
            "stale worker must not publish chunks"
        );
    }

    #[tokio::test]
    async fn failure_wire_carries_explicit_classification() {
        let (server, requests) = spawn_worker_server(vec![(200, r#"{"state":"failed"}"#)]);
        let config = test_config(server);
        let client = WorkerClient::new(&config);
        client
            .fail(&sample_lease(), "profile", "invalid profile", 1234, false)
            .await
            .unwrap();

        let request = String::from_utf8(requests.recv().unwrap()).unwrap();
        let body: Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap();
        assert_eq!(body["retryable"], false);
        assert_eq!(body["next_attempt_at_ms"], 1234);
    }
}
