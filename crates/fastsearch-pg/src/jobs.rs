use crate::error::{PgError, Result};
use crate::{sql, validate_identifier, SCHEMA_DDL_LOCK_KEY};
use fastsearch_core::AclFilter;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, Row};

/// The six authoritative states of a document-ingestion job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestState {
    Queued,
    Parsing,
    Chunking,
    Embedding,
    Indexed,
    Failed,
}

impl IngestState {
    pub const ALL: [Self; 6] = [
        Self::Queued,
        Self::Parsing,
        Self::Chunking,
        Self::Embedding,
        Self::Indexed,
        Self::Failed,
    ];

    /// Returns whether an explicit state transition is legal.
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Parsing)
                | (Self::Parsing, Self::Chunking)
                | (Self::Chunking, Self::Embedding)
                | (Self::Embedding, Self::Indexed)
                | (
                    Self::Queued | Self::Parsing | Self::Chunking | Self::Embedding,
                    Self::Failed
                )
                | (Self::Failed, Self::Queued)
        )
    }

    /// Dead-letter is deliberately derived instead of becoming a seventh state.
    pub fn is_dead_letter(self, retry_count: i32, max_retries: i32) -> bool {
        self == Self::Failed && retry_count >= max_retries
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Parsing => "parsing",
            Self::Chunking => "chunking",
            Self::Embedding => "embedding",
            Self::Indexed => "indexed",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn from_db(value: &str) -> crate::Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "parsing" => Ok(Self::Parsing),
            "chunking" => Ok(Self::Chunking),
            "embedding" => Ok(Self::Embedding),
            "indexed" => Ok(Self::Indexed),
            "failed" => Ok(Self::Failed),
            value => Err(crate::PgError::Mapping(format!(
                "unknown ingest job state: {value:?}"
            ))),
        }
    }
}

/// Deterministic exponential retry delay with ±25% stable jitter and a five-minute ceiling.
///
/// `jitter_basis` should be stable for one job/failure attempt (for example a job-id hash). This
/// keeps tests and incident replay reproducible while preventing a worker herd from retrying at
/// exactly the same instant.
pub fn retry_backoff_ms(retry_count: u32, jitter_basis: u64) -> u64 {
    const MAX_MS: u64 = 300_000;
    let multiplier = 1_u64.checked_shl(retry_count.min(63)).unwrap_or(u64::MAX);
    let nominal = 1_000_u64.saturating_mul(multiplier).min(MAX_MS);
    let permille = 750 + jitter_basis % 501;
    nominal
        .saturating_mul(permille)
        .saturating_div(1_000)
        .min(MAX_MS)
}

/// Immutable source and scheduling fields used to create one source-of-truth job row.
#[derive(Debug, Clone, PartialEq)]
pub struct NewIngestJob {
    pub job_id: String,
    pub collection: String,
    pub doc_id: String,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    pub source_uri: String,
    pub content_sha256: String,
    pub content_bytes: i64,
    pub media_type: Option<String>,
    pub filename: Option<String>,
    pub parse_profile: Value,
    pub max_retries: i32,
}

/// Persisted ingestion-job snapshot. Timestamps cross the crate boundary as Unix epoch millis.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestJob {
    pub job_id: String,
    pub collection: String,
    pub doc_id: String,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    pub source_uri: String,
    pub content_sha256: String,
    pub content_bytes: i64,
    pub media_type: Option<String>,
    pub filename: Option<String>,
    pub parse_profile: Value,
    pub state: IngestState,
    pub stage_detail: Value,
    pub chunk_count: i32,
    pub lease_owner: Option<String>,
    pub lease_epoch: i64,
    pub lease_until_ms: Option<i64>,
    pub heartbeat_at_ms: Option<i64>,
    pub retry_count: i32,
    pub max_retries: i32,
    pub next_attempt_at_ms: i64,
    pub error: Option<String>,
    pub error_stage: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

impl IngestJob {
    pub fn is_dead_letter(&self) -> bool {
        self.state
            .is_dead_letter(self.retry_count, self.max_retries)
    }
}

/// A claimed job plus the fencing identity required by every worker mutation.
#[derive(Debug, Clone, PartialEq)]
pub struct JobLease {
    pub job: IngestJob,
    pub owner: String,
    pub epoch: i64,
}

/// Result of recording a worker failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureDisposition {
    pub retry_count: i32,
    pub max_retries: i32,
    pub dead_letter: bool,
}

/// How an upload submission affected the one authoritative row for a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadDisposition {
    Created,
    Deduplicated,
    Coalesced,
    Reopened,
    Replaced,
    InFlightConflict,
}

/// Transactional upload resolution plus object-store cleanup hints.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadResolution {
    pub job: IngestJob,
    pub disposition: UploadDisposition,
    /// Newly staged bytes that were not adopted by the authoritative job.
    pub unused_source_uri: Option<String>,
    /// Previously authoritative bytes superseded by this submission.
    pub replaced_source_uri: Option<String>,
}

/// Outcome of running source/derived writes while the authoritative job row is locked and fenced.
#[derive(Debug, PartialEq, Eq)]
pub enum PublicationResult<T, E> {
    Published(T),
    FencedOut,
    WriteFailed(E),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSummary {
    pub collection: String,
    pub doc_id: String,
    pub state: IngestState,
    pub searchable: bool,
    pub chunk_count: i32,
    pub job_id: Option<String>,
    pub content_sha256: Option<String>,
    pub media_type: Option<String>,
    pub filename: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub retry_count: i32,
    pub dead_letter: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentListOptions {
    pub collection: Option<String>,
    pub state: Option<IngestState>,
    pub after_collection: Option<String>,
    pub after_doc_id: Option<String>,
    pub limit: usize,
    pub counts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestMetrics {
    pub state_counts: Vec<(IngestState, i64)>,
    pub dead_letter_count: i64,
}

impl Default for DocumentListOptions {
    fn default() -> Self {
        Self {
            collection: None,
            state: None,
            after_collection: None,
            after_doc_id: None,
            limit: 100,
            counts: false,
        }
    }
}

/// Dedicated PostgreSQL connection for ingestion scheduling and fenced state mutations.
///
/// It is deliberately separate from [`crate::PgStore`]. A worker can create one `JobStore` per
/// concurrency slot, so claim transactions never serialize through the search source-of-truth
/// connection.
pub struct JobStore {
    client: Mutex<Client>,
    table: String,
    chunks_table: String,
}

impl JobStore {
    pub async fn connect(url: &str, table: impl Into<String>) -> Result<Self> {
        Self::connect_with_chunks_table(url, table, "fastsearch_chunks").await
    }

    pub async fn connect_with_chunks_table(
        url: &str,
        table: impl Into<String>,
        chunks_table: impl Into<String>,
    ) -> Result<Self> {
        let table = table.into();
        let chunks_table = chunks_table.into();
        validate_job_identifiers(&table)?;
        validate_identifier(&chunks_table)?;
        let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("fastsearch-pg job connection error: {error}");
            }
        });
        Ok(Self {
            client: Mutex::new(client),
            table,
            chunks_table,
        })
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// Additive, idempotent schema creation serialized with the existing chunks DDL lock.
    pub async fn ensure_schema(&self) -> Result<()> {
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await?;
        transaction
            .query_one(
                &format!("SELECT pg_advisory_xact_lock({SCHEMA_DDL_LOCK_KEY})"),
                &[],
            )
            .await?;
        for statement in sql::job_ddl(&self.table) {
            if let Err(error) = transaction.batch_execute(&statement).await {
                // A failed CREATE UNIQUE INDEX leaves the transaction aborted. Roll it back
                // explicitly so this long-lived JobStore connection remains immediately usable.
                let _ = transaction.rollback().await;
                return Err(error.into());
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Inserts a new document job. FS-302 owns deduplication/overwrite policy, so a duplicate
    /// job-id or `(tenant, collection, doc_id)` is surfaced as an explicit conflict here.
    pub async fn enqueue(&self, new_job: &NewIngestJob) -> Result<IngestJob> {
        validate_new_job(new_job)?;
        let profile = serde_json::to_string(&new_job.parse_profile)?;
        let params: [&(dyn ToSql + Sync); 12] = [
            &new_job.job_id,
            &new_job.collection,
            &new_job.doc_id,
            &new_job.tenant,
            &new_job.acl,
            &new_job.source_uri,
            &new_job.content_sha256,
            &new_job.content_bytes,
            &new_job.media_type,
            &new_job.filename,
            &profile,
            &new_job.max_retries,
        ];
        let result = self
            .client
            .lock()
            .await
            .query_one(&sql::enqueue_job_sql(&self.table), &params)
            .await;
        match result {
            Ok(row) => row_to_job(&row),
            Err(error) if error.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
                Err(PgError::Conflict(format!(
                    "ingest job already exists for {}/{}",
                    new_job.collection, new_job.doc_id
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Resolve a document upload under one transaction and one document-scoped advisory lock.
    ///
    /// A document keeps one stable job row. Same-hash submissions deduplicate or coalesce;
    /// terminal/expired rows are reset in place; a different hash cannot replace active work.
    pub async fn submit_upload(&self, new_job: &NewIngestJob) -> Result<UploadResolution> {
        validate_new_job(new_job)?;
        let profile = serde_json::to_string(&new_job.parse_profile)?;
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await?;
        transaction
            .query_one(
                sql::lock_document_coordinate_sql(),
                &[&new_job.collection, &new_job.doc_id],
            )
            .await?;
        let mut existing_rows = transaction
            .query(
                &sql::lock_job_document_sql(&self.table),
                &[&new_job.collection, &new_job.doc_id],
            )
            .await?;
        if existing_rows.iter().any(|row| {
            row.try_get::<_, Option<String>>("tenant")
                .map(|tenant| tenant != new_job.tenant)
                .unwrap_or(true)
        }) {
            transaction.rollback().await?;
            return Err(PgError::Conflict(format!(
                "document {}/{} is owned by another tenant",
                new_job.collection, new_job.doc_id
            )));
        }
        reject_foreign_chunk_owner(&transaction, &self.chunks_table, new_job).await?;
        let existing = existing_rows.pop();

        let resolution = if let Some(row) = existing {
            let current = row_to_job(&row)?;
            let same_hash = current.content_sha256 == new_job.content_sha256;
            let lease_active: bool = row.try_get("lease_active")?;
            let processing = matches!(
                current.state,
                IngestState::Parsing | IngestState::Chunking | IngestState::Embedding
            );
            let active = current.state == IngestState::Queued || (processing && lease_active);

            if same_hash && current.state == IngestState::Indexed {
                UploadResolution {
                    job: current,
                    disposition: UploadDisposition::Deduplicated,
                    unused_source_uri: Some(new_job.source_uri.clone()),
                    replaced_source_uri: None,
                }
            } else if same_hash && active {
                UploadResolution {
                    job: current,
                    disposition: UploadDisposition::Coalesced,
                    unused_source_uri: Some(new_job.source_uri.clone()),
                    replaced_source_uri: None,
                }
            } else if !same_hash && active {
                UploadResolution {
                    job: current,
                    disposition: UploadDisposition::InFlightConflict,
                    unused_source_uri: Some(new_job.source_uri.clone()),
                    replaced_source_uri: None,
                }
            } else {
                let old_source_uri = (current.source_uri != new_job.source_uri)
                    .then_some(current.source_uri.clone());
                let params: [&(dyn ToSql + Sync); 9] = [
                    &current.job_id,
                    &new_job.acl,
                    &new_job.source_uri,
                    &new_job.content_sha256,
                    &new_job.content_bytes,
                    &new_job.media_type,
                    &new_job.filename,
                    &profile,
                    &new_job.max_retries,
                ];
                let row = transaction
                    .query_one(&sql::reset_upload_job_sql(&self.table), &params)
                    .await?;
                UploadResolution {
                    job: row_to_job(&row)?,
                    disposition: if same_hash {
                        UploadDisposition::Reopened
                    } else {
                        UploadDisposition::Replaced
                    },
                    unused_source_uri: None,
                    replaced_source_uri: old_source_uri,
                }
            }
        } else {
            let params: [&(dyn ToSql + Sync); 12] = [
                &new_job.job_id,
                &new_job.collection,
                &new_job.doc_id,
                &new_job.tenant,
                &new_job.acl,
                &new_job.source_uri,
                &new_job.content_sha256,
                &new_job.content_bytes,
                &new_job.media_type,
                &new_job.filename,
                &profile,
                &new_job.max_retries,
            ];
            let row = transaction
                .query_one(&sql::enqueue_job_sql(&self.table), &params)
                .await?;
            UploadResolution {
                job: row_to_job(&row)?,
                disposition: UploadDisposition::Created,
                unused_source_uri: None,
                replaced_source_uri: None,
            }
        };
        transaction.commit().await?;
        Ok(resolution)
    }

    pub async fn get(&self, job_id: &str) -> Result<Option<IngestJob>> {
        let row = self
            .client
            .lock()
            .await
            .query_opt(&sql::get_job_sql(&self.table), &[&job_id])
            .await?;
        row.as_ref().map(row_to_job).transpose()
    }

    /// Read a job only when the caller can see its tenant and ACL. Invisible and absent rows are
    /// deliberately indistinguishable.
    pub async fn get_visible(&self, job_id: &str, acl: &AclFilter) -> Result<Option<IngestJob>> {
        let sql = if acl.tenant.is_some() {
            format!(
                "SELECT {} FROM {} WHERE job_id = $1 AND tenant = $2 \
                 AND ('public' = ANY(acl) OR acl && $3)",
                sql::JOB_RETURN_COLUMNS,
                self.table
            )
        } else {
            format!(
                "SELECT {} FROM {} WHERE job_id = $1 \
                 AND ('public' = ANY(acl) OR acl && $2)",
                sql::JOB_RETURN_COLUMNS,
                self.table
            )
        };
        let client = self.client.lock().await;
        let row = if let Some(tenant) = &acl.tenant {
            client
                .query_opt(&sql, &[&job_id, tenant, &acl.allowed_tags])
                .await?
        } else {
            client
                .query_opt(&sql, &[&job_id, &acl.allowed_tags])
                .await?
        };
        row.as_ref().map(row_to_job).transpose()
    }

    /// Derive document summaries from ACL-filtered job and chunk rows. No identity or ACL field
    /// crosses this public boundary.
    pub async fn list_documents(
        &self,
        acl: &AclFilter,
        options: &DocumentListOptions,
    ) -> Result<Vec<DocumentSummary>> {
        if options.limit == 0 || options.limit > 500 {
            return Err(PgError::Config(
                "document list limit must be in 1..=500".into(),
            ));
        }
        if options.after_collection.is_some() != options.after_doc_id.is_some() {
            return Err(PgError::Config(
                "after_collection and after_doc_id must be supplied together".into(),
            ));
        }
        let state = options.state.map(IngestState::as_str);
        let limit = i64::try_from(options.limit)
            .map_err(|_| PgError::Config("document list limit is too large".into()))?;
        let tenant_scoped = acl.tenant.is_some();
        let query = document_list_sql(&self.table, &self.chunks_table, tenant_scoped);
        let client = self.client.lock().await;
        let rows = if let Some(tenant) = &acl.tenant {
            client
                .query(
                    &query,
                    &[
                        tenant,
                        &acl.allowed_tags,
                        &options.counts,
                        &options.collection,
                        &state,
                        &options.after_collection,
                        &options.after_doc_id,
                        &limit,
                    ],
                )
                .await?
        } else {
            client
                .query(
                    &query,
                    &[
                        &acl.allowed_tags,
                        &options.counts,
                        &options.collection,
                        &state,
                        &options.after_collection,
                        &options.after_doc_id,
                        &limit,
                    ],
                )
                .await?
        };
        rows.iter().map(row_to_document).collect()
    }

    pub async fn ingest_metrics(&self) -> Result<IngestMetrics> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                &format!(
                    "SELECT state, count(*)::bigint AS count FROM {} GROUP BY state",
                    self.table
                ),
                &[],
            )
            .await?;
        let mut state_counts = IngestState::ALL
            .into_iter()
            .map(|state| (state, 0))
            .collect::<Vec<_>>();
        for row in rows {
            let state = IngestState::from_db(row.try_get("state")?)?;
            if let Some((_, count)) = state_counts.iter_mut().find(|(item, _)| *item == state) {
                *count = row.try_get("count")?;
            }
        }
        let dead_letter_count = client
            .query_one(
                &format!(
                    "SELECT count(*)::bigint AS count FROM {} \
                     WHERE state = 'failed' AND retry_count >= max_retries",
                    self.table
                ),
                &[],
            )
            .await?
            .try_get("count")?;
        Ok(IngestMetrics {
            state_counts,
            dead_letter_count,
        })
    }

    /// Atomically claims up to `limit` eligible jobs using `FOR UPDATE SKIP LOCKED`.
    pub async fn claim(
        &self,
        owner: &str,
        limit: usize,
        lease_for_ms: u64,
    ) -> Result<Vec<JobLease>> {
        if owner.is_empty() {
            return Err(PgError::Config("job lease owner must not be empty".into()));
        }
        if limit == 0 || limit > 1_000 {
            return Err(PgError::Config(
                "job claim limit must be in 1..=1000".into(),
            ));
        }
        let limit = i64::try_from(limit)
            .map_err(|_| PgError::Config("job claim limit is too large".into()))?;
        let lease_for_ms = checked_duration_ms(lease_for_ms)?;
        let rows = self
            .client
            .lock()
            .await
            .query(
                &sql::claim_jobs_sql(&self.table),
                &[&limit, &owner, &lease_for_ms],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let job = row_to_job(row)?;
                Ok(JobLease {
                    epoch: job.lease_epoch,
                    owner: owner.to_owned(),
                    job,
                })
            })
            .collect()
    }

    pub async fn heartbeat(&self, lease: &JobLease, lease_for_ms: u64) -> Result<bool> {
        let lease_for_ms = checked_duration_ms(lease_for_ms)?;
        let row = self
            .client
            .lock()
            .await
            .query_opt(
                &sql::heartbeat_job_sql(&self.table),
                &[&lease.job.job_id, &lease.owner, &lease.epoch, &lease_for_ms],
            )
            .await?;
        Ok(row.is_some())
    }

    /// Advances one stage if both the state expectation and lease fence still match.
    pub async fn advance(
        &self,
        lease: &JobLease,
        expected: IngestState,
        next: IngestState,
        stage_detail: &Value,
        lease_for_ms: u64,
    ) -> Result<bool> {
        if !expected.can_transition_to(next) || next == IngestState::Failed {
            return Err(PgError::Conflict(format!(
                "invalid ingest job transition: {} -> {}",
                expected.as_str(),
                next.as_str()
            )));
        }
        let lease_for_ms = checked_duration_ms(lease_for_ms)?;
        let detail = serde_json::to_string(stage_detail)?;
        let row = self
            .client
            .lock()
            .await
            .query_opt(
                &sql::advance_job_sql(&self.table),
                &[
                    &lease.job.job_id,
                    &lease.owner,
                    &lease.epoch,
                    &expected.as_str(),
                    &next.as_str(),
                    &detail,
                    &lease_for_ms,
                ],
            )
            .await?;
        Ok(row.is_some())
    }

    /// Marks an embedding-stage job indexed. Call only after all source and derived writes commit.
    pub async fn finish(&self, lease: &JobLease, chunk_count: i32) -> Result<bool> {
        if chunk_count < 0 {
            return Err(PgError::Config("chunk_count must be non-negative".into()));
        }
        let row = self
            .client
            .lock()
            .await
            .query_opt(
                &sql::finish_job_sql(&self.table),
                &[&lease.job.job_id, &lease.owner, &lease.epoch, &chunk_count],
            )
            .await?;
        Ok(row.is_some())
    }

    /// Hold the job row lock across the supplied write future, then publish `indexed` in the same
    /// transaction. A competing reclaim blocks on the row lock, so a lease cannot expire and be
    /// reassigned between the last fence check and source/derived writes.
    pub async fn publish_indexed<F, Fut, T, E>(
        &self,
        lease: &JobLease,
        chunk_count: i32,
        write: F,
    ) -> Result<PublicationResult<T, E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
    {
        if chunk_count < 0 {
            return Err(PgError::Config("chunk_count must be non-negative".into()));
        }
        let mut client = self.client.lock().await;
        let transaction = client.transaction().await?;
        let fenced = transaction
            .query_opt(
                &format!(
                    "SELECT job_id FROM {} WHERE job_id = $1 AND lease_owner = $2 \
                     AND lease_epoch = $3 AND state = 'embedding' \
                     AND lease_until >= clock_timestamp() FOR UPDATE",
                    self.table
                ),
                &[&lease.job.job_id, &lease.owner, &lease.epoch],
            )
            .await?;
        if fenced.is_none() {
            transaction.rollback().await?;
            return Ok(PublicationResult::FencedOut);
        }
        let value = match write().await {
            Ok(value) => value,
            Err(error) => {
                transaction.rollback().await?;
                return Ok(PublicationResult::WriteFailed(error));
            }
        };
        transaction
            .query_one(
                &format!(
                    "UPDATE {} SET state = 'indexed', chunk_count = $4, error = NULL, \
                     error_stage = NULL, lease_owner = NULL, lease_until = NULL, \
                     finished_at = clock_timestamp(), updated_at = clock_timestamp() \
                     WHERE job_id = $1 AND lease_owner = $2 AND lease_epoch = $3 \
                     AND state = 'embedding' RETURNING job_id",
                    self.table
                ),
                &[&lease.job.job_id, &lease.owner, &lease.epoch, &chunk_count],
            )
            .await?;
        transaction.commit().await?;
        Ok(PublicationResult::Published(value))
    }

    /// Records a failure and a caller-computed next-attempt timestamp.
    pub async fn fail(
        &self,
        lease: &JobLease,
        error: &str,
        error_stage: &str,
        next_attempt_at_ms: i64,
    ) -> Result<Option<FailureDisposition>> {
        let row = self
            .client
            .lock()
            .await
            .query_opt(
                &sql::fail_job_sql(&self.table),
                &[
                    &lease.job.job_id,
                    &lease.owner,
                    &lease.epoch,
                    &error,
                    &error_stage,
                    &next_attempt_at_ms,
                ],
            )
            .await?;
        Ok(row.map(|row| {
            let retry_count = row.get("retry_count");
            let max_retries = row.get("max_retries");
            FailureDisposition {
                retry_count,
                max_retries,
                dead_letter: retry_count >= max_retries,
            }
        }))
    }
}

fn validate_job_identifiers(table: &str) -> Result<()> {
    validate_identifier(table)?;
    for suffix in [
        "_state_check",
        "_retry_check",
        "_doc",
        "_global_doc",
        "_claim",
        "_list",
        "_hash",
    ] {
        validate_identifier(&format!("{table}{suffix}"))?;
    }
    Ok(())
}

fn validate_new_job(job: &NewIngestJob) -> Result<()> {
    if job.job_id.is_empty()
        || job.collection.is_empty()
        || job.doc_id.is_empty()
        || job.source_uri.is_empty()
        || job.content_sha256.is_empty()
    {
        return Err(PgError::Config(
            "job_id, collection, doc_id, source_uri and content_sha256 must not be empty".into(),
        ));
    }
    if job.content_bytes < 0 || job.max_retries <= 0 {
        return Err(PgError::Config(
            "content_bytes must be non-negative and max_retries must be positive".into(),
        ));
    }
    if job.acl.is_empty() || job.acl.iter().any(String::is_empty) {
        return Err(PgError::Config(
            "job ACL must contain at least one non-empty tag".into(),
        ));
    }
    if job.content_sha256.len() != 64
        || !job
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(PgError::Config(
            "content_sha256 must be a full 64-character hexadecimal SHA-256".into(),
        ));
    }
    if !job.parse_profile.is_object() {
        return Err(PgError::Config(
            "parse_profile must be a JSON object".into(),
        ));
    }
    Ok(())
}

async fn reject_foreign_chunk_owner(
    transaction: &tokio_postgres::Transaction<'_>,
    chunks_table: &str,
    job: &NewIngestJob,
) -> Result<()> {
    let exists: bool = transaction
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&chunks_table])
        .await?
        .get(0);
    if !exists {
        return Ok(());
    }
    let owners = transaction
        .query(
            &sql::lock_doc_ownership_sql(chunks_table),
            &[&job.collection, &job.doc_id],
        )
        .await?;
    if owners.iter().any(|row| {
        row.try_get::<_, Option<String>>("tenant")
            .map(|tenant| tenant != job.tenant)
            .unwrap_or(true)
    }) {
        return Err(PgError::Conflict(format!(
            "document {}/{} is owned by another tenant",
            job.collection, job.doc_id
        )));
    }
    Ok(())
}

fn checked_duration_ms(value: u64) -> Result<i64> {
    if value == 0 {
        return Err(PgError::Config("lease duration must be positive".into()));
    }
    i64::try_from(value).map_err(|_| PgError::Config("lease duration is too large".into()))
}

fn document_list_sql(jobs_table: &str, chunks_table: &str, tenant_scoped: bool) -> String {
    let (job_acl, chunk_acl, counts, collection, state, after_collection, after_doc, limit) =
        if tenant_scoped {
            (
                "tenant = $1 AND ('public' = ANY(acl) OR acl && $2)",
                "tenant = $1 AND ('public' = ANY(acl) OR acl && $2)",
                "$3",
                "$4",
                "$5",
                "$6",
                "$7",
                "$8",
            )
        } else {
            (
                "('public' = ANY(acl) OR acl && $1)",
                "('public' = ANY(acl) OR acl && $1)",
                "$2",
                "$3",
                "$4",
                "$5",
                "$6",
                "$7",
            )
        };
    let visible_jobs = if tenant_scoped {
        format!("SELECT * FROM {jobs_table} WHERE {job_acl}")
    } else {
        // Public document identity is GlobalId without tenant. Historical rows created before
        // global ownership enforcement are collapsed deterministically so the public two-field
        // keyset remains total and never skips an equal coordinate at a page boundary.
        format!(
            "SELECT DISTINCT ON (collection, doc_id) * FROM {jobs_table} WHERE {job_acl} \
             ORDER BY collection, doc_id, updated_at DESC, job_id"
        )
    };
    let (chunk_tenant, chunk_group, join_identity) = if tenant_scoped {
        (
            "tenant",
            "tenant, collection, doc_id",
            "j.tenant IS NOT DISTINCT FROM c.tenant AND j.collection = c.collection AND j.doc_id = c.doc_id",
        )
    } else {
        (
            "NULL::text AS tenant",
            "collection, doc_id",
            "j.collection = c.collection AND j.doc_id = c.doc_id",
        )
    };
    format!(
        "WITH visible_jobs AS (\
           {visible_jobs}\
         ), visible_chunks AS (\
           SELECT {chunk_tenant}, collection, doc_id, bool_or(searchable) AS searchable, \
                  CASE WHEN {counts}::boolean THEN count(*)::integer ELSE 0 END AS chunk_count, \
                  min(updated_at) AS created_at, max(updated_at) AS updated_at \
             FROM {chunks_table} WHERE {chunk_acl} GROUP BY {chunk_group}\
         ), documents AS (\
           SELECT COALESCE(j.collection, c.collection) AS collection, \
                  COALESCE(j.doc_id, c.doc_id) AS doc_id, \
                  COALESCE(j.state, 'indexed') AS state, \
                  CASE WHEN j.job_id IS NULL THEN c.searchable \
                       WHEN j.state = 'indexed' THEN COALESCE(c.searchable, false) ELSE false END AS searchable, \
                  COALESCE(c.chunk_count, j.chunk_count, 0) AS chunk_count, j.job_id, \
                  j.content_sha256, j.media_type, j.filename, \
                  (extract(epoch FROM COALESCE(j.created_at, c.created_at)) * 1000)::bigint AS created_at_ms, \
                  (extract(epoch FROM GREATEST(j.updated_at, c.updated_at)) * 1000)::bigint AS updated_at_ms, \
                  (extract(epoch FROM j.finished_at) * 1000)::bigint AS finished_at_ms, \
                  COALESCE(j.retry_count, 0) AS retry_count, \
                  COALESCE(j.state = 'failed' AND j.retry_count >= j.max_retries, false) AS dead_letter, \
                  j.error \
             FROM visible_jobs j FULL OUTER JOIN visible_chunks c \
               ON {join_identity}\
         ) SELECT collection, doc_id, state, searchable, chunk_count, job_id, content_sha256, \
                  media_type, filename, created_at_ms, updated_at_ms, finished_at_ms, retry_count, \
                  dead_letter, error FROM documents \
           WHERE ({collection}::text IS NULL OR collection = {collection}) \
             AND ({state}::text IS NULL OR state = {state}) \
             AND ({after_collection}::text IS NULL OR \
                  (collection, doc_id) > ({after_collection}::text, {after_doc}::text)) \
           ORDER BY collection, doc_id LIMIT {limit}::bigint"
    )
}

fn row_to_job(row: &Row) -> Result<IngestJob> {
    let parse_profile: String = row.try_get("parse_profile")?;
    let stage_detail: String = row.try_get("stage_detail")?;
    let state: String = row.try_get("state")?;
    Ok(IngestJob {
        job_id: row.try_get("job_id")?,
        collection: row.try_get("collection")?,
        doc_id: row.try_get("doc_id")?,
        tenant: row.try_get("tenant")?,
        acl: row.try_get("acl")?,
        source_uri: row.try_get("source_uri")?,
        content_sha256: row.try_get("content_sha256")?,
        content_bytes: row.try_get("content_bytes")?,
        media_type: row.try_get("media_type")?,
        filename: row.try_get("filename")?,
        parse_profile: serde_json::from_str(&parse_profile)?,
        state: IngestState::from_db(&state)?,
        stage_detail: serde_json::from_str(&stage_detail)?,
        chunk_count: row.try_get("chunk_count")?,
        lease_owner: row.try_get("lease_owner")?,
        lease_epoch: row.try_get("lease_epoch")?,
        lease_until_ms: row.try_get("lease_until_ms")?,
        heartbeat_at_ms: row.try_get("heartbeat_at_ms")?,
        retry_count: row.try_get("retry_count")?,
        max_retries: row.try_get("max_retries")?,
        next_attempt_at_ms: row.try_get("next_attempt_at_ms")?,
        error: row.try_get("error")?,
        error_stage: row.try_get("error_stage")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        started_at_ms: row.try_get("started_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
    })
}

fn row_to_document(row: &Row) -> Result<DocumentSummary> {
    let state: String = row.try_get("state")?;
    Ok(DocumentSummary {
        collection: row.try_get("collection")?,
        doc_id: row.try_get("doc_id")?,
        state: IngestState::from_db(&state)?,
        searchable: row.try_get("searchable")?,
        chunk_count: row.try_get("chunk_count")?,
        job_id: row.try_get("job_id")?,
        content_sha256: row.try_get("content_sha256")?,
        media_type: row.try_get("media_type")?,
        filename: row.try_get("filename")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        retry_count: row.try_get("retry_count")?,
        dead_letter: row.try_get("dead_letter")?,
        error: row.try_get("error")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_job(job_id: &str, doc_id: &str, max_retries: i32) -> NewIngestJob {
        NewIngestJob {
            job_id: job_id.into(),
            collection: "kb".into(),
            doc_id: doc_id.into(),
            tenant: Some("acme".into()),
            acl: vec!["team-a".into()],
            source_uri: format!("local://acme/kb/{doc_id}"),
            content_sha256: "a".repeat(64),
            content_bytes: 42,
            media_type: Some("text/markdown".into()),
            filename: Some(doc_id.into()),
            parse_profile: serde_json::json!({"chunking": {"target_chars": 800}}),
            max_retries,
        }
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis() as i64
    }

    fn database_url_with_name(url: &str, database: &str) -> String {
        let (base, query) = url
            .split_once('?')
            .map_or((url, ""), |(base, query)| (base, query));
        let slash = base.rfind('/').expect("DATABASE_URL must contain a path");
        let suffix = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        format!("{}{database}{suffix}", &base[..=slash])
    }

    #[test]
    fn state_machine_accepts_only_authoritative_transitions() {
        use IngestState::{Chunking, Embedding, Failed, Indexed, Parsing, Queued};

        let valid = [
            (Queued, Parsing),
            (Parsing, Chunking),
            (Chunking, Embedding),
            (Embedding, Indexed),
            (Queued, Failed),
            (Parsing, Failed),
            (Chunking, Failed),
            (Embedding, Failed),
            (Failed, Queued),
        ];
        for (from, to) in valid {
            assert!(from.can_transition_to(to), "{from:?} -> {to:?}");
        }

        for from in IngestState::ALL {
            for to in IngestState::ALL {
                let expected = valid.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "unexpected transition verdict for {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn dead_letter_is_derived_without_a_seventh_state() {
        assert!(!IngestState::Failed.is_dead_letter(2, 3));
        assert!(IngestState::Failed.is_dead_letter(3, 3));
        assert!(IngestState::Failed.is_dead_letter(4, 3));
        assert!(!IngestState::Queued.is_dead_letter(3, 3));
        assert_eq!(IngestState::ALL.len(), 6);
    }

    #[test]
    fn retry_backoff_is_deterministic_jittered_and_capped() {
        assert_eq!(retry_backoff_ms(4, 42), retry_backoff_ms(4, 42));
        for retry in 0..20 {
            let nominal = 1_000_u64.saturating_mul(1_u64 << retry.min(18));
            let delay = retry_backoff_ms(retry, 77);
            assert!(delay <= 300_000);
            if nominal <= 240_000 {
                assert!(delay >= nominal * 3 / 4);
                assert!(delay <= nominal * 5 / 4);
            }
        }
        assert_eq!(retry_backoff_ms(63, 500), 300_000);
    }

    #[tokio::test]
    async fn job_store_rejects_invalid_or_overlong_derived_identifiers_before_connecting() {
        assert!(matches!(
            JobStore::connect("postgres://unused", "jobs;drop").await,
            Err(crate::PgError::Config(_))
        ));
        let parent_valid_but_constraint_too_long = format!("j{}", "x".repeat(51));
        assert!(matches!(
            JobStore::connect("postgres://unused", parent_valid_but_constraint_too_long).await,
            Err(crate::PgError::Config(_))
        ));
    }

    #[test]
    fn new_job_validation_fails_closed_on_unclaimable_or_untrusted_inputs() {
        let mut job = sample_job("job", "doc.md", 3);
        assert!(validate_new_job(&job).is_ok());

        job.max_retries = 0;
        assert!(validate_new_job(&job).is_err());
        job.max_retries = 3;
        job.acl.clear();
        assert!(validate_new_job(&job).is_err());
        job.acl.push("team-a".into());
        job.content_sha256 = "not-a-full-sha".into();
        assert!(validate_new_job(&job).is_err());
    }

    #[tokio::test]
    async fn fs301_concurrent_claims_are_disjoint_and_stage_writes_are_fenced() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs301_concurrent_claims_are_disjoint_and_stage_writes_are_fenced: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fs301_claim_{}", std::process::id());
        let seed = JobStore::connect(&url, &table).await.expect("seed connect");
        seed.client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE IF EXISTS {table} CASCADE;"))
            .await
            .expect("clean");
        seed.ensure_schema().await.expect("schema");
        for index in 0..4 {
            seed.enqueue(&sample_job(
                &format!("claim-{index}"),
                &format!("claim-{index}.md"),
                3,
            ))
            .await
            .expect("enqueue");
        }

        let first = JobStore::connect(&url, &table)
            .await
            .expect("first connect");
        let second = JobStore::connect(&url, &table)
            .await
            .expect("second connect");
        let (left, right) = tokio::join!(
            first.claim("worker-a", 2, 10_000),
            second.claim("worker-b", 2, 10_000)
        );
        let left = left.expect("first claim");
        let right = right.expect("second claim");
        assert_eq!(left.len(), 2);
        assert_eq!(right.len(), 2);
        let left_ids = left
            .iter()
            .map(|lease| lease.job.job_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert!(right
            .iter()
            .all(|lease| !left_ids.contains(lease.job.job_id.as_str())));

        let lease = &left[0];
        assert!(first
            .advance(
                lease,
                IngestState::Parsing,
                IngestState::Chunking,
                &serde_json::json!({"chunks_parsed": 4}),
                10_000,
            )
            .await
            .expect("advance chunking"));
        assert!(first
            .advance(
                lease,
                IngestState::Chunking,
                IngestState::Embedding,
                &serde_json::json!({"chunks_parsed": 4}),
                10_000,
            )
            .await
            .expect("advance embedding"));
        let mut forged = lease.clone();
        forged.owner = "worker-b".into();
        assert!(!second.finish(&forged, 4).await.expect("fenced finish"));
        assert!(first.finish(lease, 4).await.expect("finish"));
        let finished = first
            .get(&lease.job.job_id)
            .await
            .expect("get")
            .expect("job");
        assert_eq!(finished.state, IngestState::Indexed);
        assert_eq!(finished.chunk_count, 4);
        assert!(finished.finished_at_ms.is_some());

        seed.client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE {table} CASCADE;"))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs301_expired_lease_is_reclaimed_and_retry_exhaustion_is_dead_letter() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs301_expired_lease_is_reclaimed_and_retry_exhaustion_is_dead_letter: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fs301_lease_{}", std::process::id());
        let first = JobStore::connect(&url, &table)
            .await
            .expect("first connect");
        first
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE IF EXISTS {table} CASCADE;"))
            .await
            .expect("clean");
        first.ensure_schema().await.expect("schema");
        first
            .enqueue(&sample_job("lease-job", "lease.md", 1))
            .await
            .expect("enqueue");
        let stale = first
            .claim("worker-old", 1, 50)
            .await
            .expect("old claim")
            .pop()
            .expect("lease");
        tokio::time::sleep(std::time::Duration::from_millis(125)).await;
        assert!(!first
            .heartbeat(&stale, 10_000)
            .await
            .expect("expired heartbeat"));

        let second = JobStore::connect(&url, &table)
            .await
            .expect("second connect");
        let current = second
            .claim("worker-new", 1, 10_000)
            .await
            .expect("reclaim")
            .pop()
            .expect("new lease");
        assert_eq!(current.epoch, stale.epoch + 1);
        assert!(!first
            .advance(
                &stale,
                IngestState::Parsing,
                IngestState::Chunking,
                &serde_json::json!({}),
                10_000,
            )
            .await
            .expect("stale advance"));
        assert!(second
            .advance(
                &current,
                IngestState::Parsing,
                IngestState::Chunking,
                &serde_json::json!({}),
                10_000,
            )
            .await
            .expect("current chunking"));
        assert!(second
            .advance(
                &current,
                IngestState::Chunking,
                IngestState::Embedding,
                &serde_json::json!({}),
                10_000,
            )
            .await
            .expect("current embedding"));
        assert!(!first.finish(&stale, 1).await.expect("stale finish"));
        assert!(first
            .fail(&stale, "late failure", "embedding", now_ms() - 1)
            .await
            .expect("stale fail")
            .is_none());

        let failure = second
            .fail(&current, "embed unavailable", "embedding", now_ms() - 1)
            .await
            .expect("fail")
            .expect("current lease accepted");
        assert_eq!(failure.retry_count, 1);
        assert!(failure.dead_letter);
        let persisted = second.get("lease-job").await.expect("get").expect("job");
        assert!(persisted.is_dead_letter());
        assert!(second
            .claim("worker-third", 1, 10_000)
            .await
            .expect("dead letter claim")
            .is_empty());

        first
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE {table} CASCADE;"))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs302_upload_submission_is_idempotent_and_preserves_one_job_row() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs302_upload_submission_is_idempotent_and_preserves_one_job_row: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fs302_submit_{}", std::process::id());
        let store = JobStore::connect(&url, &table).await.expect("connect");
        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE IF EXISTS {table} CASCADE;"))
            .await
            .expect("clean");
        store.ensure_schema().await.expect("schema");

        let original = sample_job("upload-job", "upload.md", 3);
        let created = store.submit_upload(&original).await.expect("create");
        assert_eq!(created.disposition, UploadDisposition::Created);
        assert_eq!(created.job.job_id, "upload-job");

        let mut duplicate = original.clone();
        duplicate.job_id = "must-not-create-a-second-row".into();
        duplicate.source_uri = "local://staged/duplicate".into();
        let coalesced = store.submit_upload(&duplicate).await.expect("coalesce");
        assert_eq!(coalesced.disposition, UploadDisposition::Coalesced);
        assert_eq!(coalesced.job.job_id, "upload-job");
        assert_eq!(
            coalesced.unused_source_uri.as_deref(),
            Some("local://staged/duplicate")
        );

        let mut changed = duplicate.clone();
        changed.content_sha256 = "b".repeat(64);
        changed.source_uri = "local://staged/changed".into();
        let conflict = store
            .submit_upload(&changed)
            .await
            .expect("conflict result");
        assert_eq!(conflict.disposition, UploadDisposition::InFlightConflict);
        assert_eq!(conflict.job.job_id, "upload-job");
        assert_eq!(
            conflict.unused_source_uri.as_deref(),
            Some("local://staged/changed")
        );

        let lease = store
            .claim("worker", 1, 10_000)
            .await
            .expect("claim")
            .pop()
            .expect("lease");
        store
            .advance(
                &lease,
                IngestState::Parsing,
                IngestState::Chunking,
                &serde_json::json!({}),
                10_000,
            )
            .await
            .expect("chunking");
        store
            .advance(
                &lease,
                IngestState::Chunking,
                IngestState::Embedding,
                &serde_json::json!({}),
                10_000,
            )
            .await
            .expect("embedding");
        assert!(store.finish(&lease, 2).await.expect("finish"));

        let indexed_duplicate = store
            .submit_upload(&duplicate)
            .await
            .expect("deduplicate indexed");
        assert_eq!(
            indexed_duplicate.disposition,
            UploadDisposition::Deduplicated
        );
        assert_eq!(indexed_duplicate.job.chunk_count, 2);

        let replaced = store.submit_upload(&changed).await.expect("replace");
        assert_eq!(replaced.disposition, UploadDisposition::Replaced);
        assert_eq!(replaced.job.job_id, "upload-job");
        assert_eq!(replaced.job.content_sha256, "b".repeat(64));
        assert_eq!(
            replaced.replaced_source_uri.as_deref(),
            Some(original.source_uri.as_str())
        );
        assert_eq!(replaced.job.state, IngestState::Queued);
        assert_eq!(replaced.job.retry_count, 0);
        assert_eq!(replaced.job.chunk_count, 0);

        let failed_lease = store
            .claim("worker-reopen", 1, 10_000)
            .await
            .expect("claim replacement")
            .pop()
            .expect("replacement lease");
        assert!(store
            .fail(&failed_lease, "parse failed", "parsing", now_ms())
            .await
            .expect("fail replacement")
            .is_some());
        let reopened = store
            .submit_upload(&changed)
            .await
            .expect("reopen same failed content");
        assert_eq!(reopened.disposition, UploadDisposition::Reopened);
        assert_eq!(reopened.job.state, IngestState::Queued);

        let mut foreign = original.clone();
        foreign.job_id = "foreign-job".into();
        foreign.tenant = Some("other-tenant".into());
        foreign.acl = vec!["other-team".into()];
        let error = store
            .submit_upload(&foreign)
            .await
            .expect_err("global document coordinate cannot change tenant ownership");
        assert!(matches!(error, PgError::Conflict(_)));

        let peer = JobStore::connect(&url, &table).await.expect("peer connect");
        let mut race_a = sample_job("race-a", "race.md", 3);
        race_a.source_uri = "local://staged/race-a".into();
        let mut race_b = race_a.clone();
        race_b.job_id = "race-b".into();
        race_b.source_uri = "local://staged/race-b".into();
        let (a, b) = tokio::join!(store.submit_upload(&race_a), peer.submit_upload(&race_b));
        let dispositions = [
            a.expect("race a").disposition,
            b.expect("race b").disposition,
        ];
        assert!(dispositions.contains(&UploadDisposition::Created));
        assert!(dispositions.contains(&UploadDisposition::Coalesced));
        let race_rows: i64 = store
            .client
            .lock()
            .await
            .query_one(
                &format!("SELECT count(*)::bigint FROM {table} WHERE collection='kb' AND doc_id='race.md'"),
                &[],
            )
            .await
            .expect("count race rows")
            .get(0);
        assert_eq!(race_rows, 1);

        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE {table} CASCADE;"))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs302_job_and_document_reads_apply_acl_before_deriving_results() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs302_job_and_document_reads_apply_acl_before_deriving_results: DATABASE_URL not set"
            );
            return;
        };
        let suffix = std::process::id();
        let jobs = format!("fs302_read_jobs_{suffix}");
        let chunks = format!("fs302_read_chunks_{suffix}");
        let store = JobStore::connect_with_chunks_table(&url, &jobs, &chunks)
            .await
            .expect("connect");
        store
            .client
            .lock()
            .await
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {jobs} CASCADE; DROP TABLE IF EXISTS {chunks} CASCADE; \
                 CREATE TABLE {chunks} (collection text NOT NULL, doc_id text NOT NULL, \
                 tenant text, acl text[] NOT NULL, searchable boolean NOT NULL, \
                 updated_at timestamptz NOT NULL DEFAULT now());"
            ))
            .await
            .expect("clean");
        store.ensure_schema().await.expect("schema");
        store
            .submit_upload(&sample_job("visible-job", "job-only.md", 3))
            .await
            .expect("visible job");
        let mut hidden = sample_job("hidden-job", "hidden.md", 3);
        hidden.acl = vec!["team-b".into()];
        store.submit_upload(&hidden).await.expect("hidden job");
        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP INDEX {jobs}_global_doc;"))
            .await
            .expect("simulate legacy index");
        store
            .client
            .lock()
            .await
            .execute(
                &format!(
                    "INSERT INTO {chunks} (collection, doc_id, tenant, acl, searchable) VALUES \
                     ('kb', 'chunk-only.md', 'acme', ARRAY['team-a'], true), \
                     ('kb', 'job-only.md', 'acme', ARRAY['team-a'], true), \
                     ('kb', 'hidden-chunk.md', 'acme', ARRAY['team-b'], true)"
                ),
                &[],
            )
            .await
            .expect("chunks");

        let allowed = fastsearch_core::AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-a".into()],
        };
        let denied = fastsearch_core::AclFilter {
            tenant: Some("acme".into()),
            allowed_tags: vec!["team-b".into()],
        };
        assert!(store
            .get_visible("visible-job", &allowed)
            .await
            .expect("visible get")
            .is_some());
        assert!(store
            .get_visible("visible-job", &denied)
            .await
            .expect("invisible get")
            .is_none());

        let docs = store
            .list_documents(
                &allowed,
                &DocumentListOptions {
                    collection: Some("kb".into()),
                    limit: 100,
                    counts: true,
                    ..DocumentListOptions::default()
                },
            )
            .await
            .expect("list");
        assert_eq!(
            docs.iter()
                .map(|doc| doc.doc_id.as_str())
                .collect::<Vec<_>>(),
            vec!["chunk-only.md", "job-only.md"]
        );
        assert_eq!(docs[0].state, IngestState::Indexed);
        assert_eq!(docs[0].chunk_count, 1);
        assert!(docs[0].job_id.is_none());
        assert_eq!(docs[1].state, IngestState::Queued);
        assert_eq!(docs[1].chunk_count, 1);
        assert_eq!(docs[1].job_id.as_deref(), Some("visible-job"));
        assert!(docs.iter().all(|doc| !doc.doc_id.starts_with("hidden")));

        // Simulate a historical pre-FS-302 duplicate coordinate. An unscoped administrator must
        // still receive a total two-field keyset, not two indistinguishable cursors that skip.
        store
            .client
            .lock()
            .await
            .execute(
                &format!(
                    "INSERT INTO {jobs} (job_id, collection, doc_id, tenant, acl, source_uri, \
                     content_sha256, content_bytes) VALUES \
                     ('legacy-other-tenant', 'kb', 'job-only.md', 'legacy-tenant', \
                      ARRAY['team-a'], 'local://legacy', $1, 1)"
                ),
                &[&"c".repeat(64)],
            )
            .await
            .expect("legacy duplicate");
        let admin = fastsearch_core::AclFilter {
            tenant: None,
            allowed_tags: vec!["team-a".into()],
        };
        let first = store
            .list_documents(
                &admin,
                &DocumentListOptions {
                    collection: Some("kb".into()),
                    limit: 1,
                    ..DocumentListOptions::default()
                },
            )
            .await
            .expect("admin first page");
        assert_eq!(first.len(), 1);
        let second = store
            .list_documents(
                &admin,
                &DocumentListOptions {
                    collection: Some("kb".into()),
                    after_collection: Some(first[0].collection.clone()),
                    after_doc_id: Some(first[0].doc_id.clone()),
                    limit: 1,
                    ..DocumentListOptions::default()
                },
            )
            .await
            .expect("admin second page");
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].doc_id, second[0].doc_id);
        let exhausted = store
            .list_documents(
                &admin,
                &DocumentListOptions {
                    collection: Some("kb".into()),
                    after_collection: Some(second[0].collection.clone()),
                    after_doc_id: Some(second[0].doc_id.clone()),
                    limit: 1,
                    ..DocumentListOptions::default()
                },
            )
            .await
            .expect("admin exhausted page");
        assert!(exhausted.is_empty());

        // The global uniqueness upgrade must fail closed when historical tenant-scoped rows
        // collide. Because ensure_schema is transactional, neither row may be deleted or changed
        // and the new index must not appear partially.
        store
            .ensure_schema()
            .await
            .expect_err("global-coordinate upgrade must reject historical duplicates");
        let client = store.client.lock().await;
        let duplicate_count: i64 = client
            .query_one(
                &format!(
                    "SELECT count(*) FROM {jobs} WHERE collection = 'kb' AND doc_id = 'job-only.md'"
                ),
                &[],
            )
            .await
            .expect("duplicate rows remain query")
            .get(0);
        assert_eq!(duplicate_count, 2);
        let global_index: Option<String> = client
            .query_one(
                "SELECT to_regclass($1)::text",
                &[&format!("{jobs}_global_doc")],
            )
            .await
            .expect("global index presence query")
            .get(0);
        assert!(global_index.is_none());
        drop(client);

        store
            .client
            .lock()
            .await
            .batch_execute(&format!(
                "DROP TABLE {jobs} CASCADE; DROP TABLE {chunks} CASCADE;"
            ))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs302_publication_lock_blocks_reclaim_and_failure_never_marks_indexed() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs302_publication_lock_blocks_reclaim_and_failure_never_marks_indexed: DATABASE_URL not set"
            );
            return;
        };
        let table = format!("fs302_publish_{}", std::process::id());
        let store = std::sync::Arc::new(JobStore::connect(&url, &table).await.expect("connect"));
        let peer = JobStore::connect(&url, &table).await.expect("peer connect");
        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE IF EXISTS {table} CASCADE;"))
            .await
            .expect("clean");
        store.ensure_schema().await.expect("schema");
        store
            .submit_upload(&sample_job("publish-job", "publish.md", 3))
            .await
            .expect("submit");
        let lease = store
            .claim("worker-a", 1, 100)
            .await
            .expect("claim")
            .pop()
            .expect("lease");
        store
            .advance(
                &lease,
                IngestState::Parsing,
                IngestState::Chunking,
                &serde_json::json!({}),
                100,
            )
            .await
            .expect("chunking");
        store
            .advance(
                &lease,
                IngestState::Chunking,
                IngestState::Embedding,
                &serde_json::json!({}),
                100,
            )
            .await
            .expect("embedding");

        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let publishing = {
            let store = store.clone();
            let entered = entered.clone();
            let release = release.clone();
            let lease = lease.clone();
            tokio::spawn(async move {
                store
                    .publish_indexed(&lease, 1, || async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok::<_, &'static str>(())
                    })
                    .await
                    .expect("publication")
            })
        };
        entered.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            peer.claim("worker-b", 1, 1_000)
                .await
                .expect("reclaim while published row locked")
                .is_empty(),
            "SKIP LOCKED must not reclaim an expired job during publication"
        );
        release.notify_one();
        assert_eq!(
            publishing.await.expect("publisher join"),
            PublicationResult::Published(())
        );
        assert_eq!(
            store.get("publish-job").await.unwrap().unwrap().state,
            IngestState::Indexed
        );
        assert!(peer
            .claim("worker-b", 1, 1_000)
            .await
            .expect("indexed not claimable")
            .is_empty());

        store
            .submit_upload(&sample_job("failed-publish", "failed-publish.md", 3))
            .await
            .expect("submit failed publication");
        let failed_lease = peer
            .claim("worker-c", 1, 10_000)
            .await
            .expect("claim failed publication")
            .pop()
            .expect("failed publication lease");
        peer.advance(
            &failed_lease,
            IngestState::Parsing,
            IngestState::Chunking,
            &serde_json::json!({}),
            10_000,
        )
        .await
        .expect("failed publication chunking");
        peer.advance(
            &failed_lease,
            IngestState::Chunking,
            IngestState::Embedding,
            &serde_json::json!({}),
            10_000,
        )
        .await
        .expect("failed publication embedding");
        assert_eq!(
            peer.publish_indexed(&failed_lease, 1, || async { Err::<(), _>("write failed") })
                .await
                .expect("failed publication result"),
            PublicationResult::WriteFailed("write failed")
        );
        assert_eq!(
            peer.get("failed-publish").await.unwrap().unwrap().state,
            IngestState::Embedding
        );

        store
            .client
            .lock()
            .await
            .batch_execute(&format!("DROP TABLE {table} CASCADE;"))
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn fs301_additive_upgrade_and_chunks_only_publication_hold_in_real_pg() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skip fs301_additive_upgrade_and_chunks_only_publication_hold_in_real_pg: DATABASE_URL not set"
            );
            return;
        };
        let database = format!("fastsearch_fs301_pub_{}", std::process::id());
        let (admin, admin_connection) = tokio_postgres::connect(&url, NoTls)
            .await
            .expect("admin connect");
        tokio::spawn(async move {
            let _ = admin_connection.await;
        });
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE);"))
            .await
            .expect("drop stale database");
        admin
            .batch_execute(&format!("CREATE DATABASE {database};"))
            .await
            .expect("create private database");
        let isolated_url = database_url_with_name(&url, &database);
        let jobs_table = "fastsearch_ingest_jobs_it";
        let (raw, raw_connection) = tokio_postgres::connect(&isolated_url, NoTls)
            .await
            .expect("raw connect");
        tokio::spawn(async move {
            let _ = raw_connection.await;
        });
        raw.batch_execute(&format!(
            "CREATE TABLE {jobs_table} (\
               job_id text PRIMARY KEY, collection text NOT NULL, doc_id text NOT NULL, \
               tenant text, acl text[] NOT NULL, source_uri text NOT NULL, \
               content_sha256 text NOT NULL, content_bytes bigint NOT NULL, \
               state text NOT NULL DEFAULT 'queued'\
             ); \
             INSERT INTO {jobs_table} \
               (job_id,collection,doc_id,tenant,acl,source_uri,content_sha256,content_bytes) \
             VALUES ('old-job','kb','old.md','acme',ARRAY['team-a'],'local://old','old-sha',7);"
        ))
        .await
        .expect("old schema fixture");

        let one = JobStore::connect(&isolated_url, jobs_table)
            .await
            .expect("job connect one");
        let two = JobStore::connect(&isolated_url, jobs_table)
            .await
            .expect("job connect two");
        let three = JobStore::connect(&isolated_url, jobs_table)
            .await
            .expect("job connect three");
        let (a, b, c) = tokio::join!(
            one.ensure_schema(),
            two.ensure_schema(),
            three.ensure_schema()
        );
        a.expect("concurrent schema one");
        b.expect("concurrent schema two");
        c.expect("concurrent schema three");
        let upgraded = one
            .get("old-job")
            .await
            .expect("get upgraded")
            .expect("old row preserved");
        assert_eq!(upgraded.state, IngestState::Queued);
        assert_eq!(upgraded.stage_detail, serde_json::json!({}));
        assert_eq!(upgraded.max_retries, 3);
        assert_eq!(upgraded.acl, vec!["team-a"]);

        let mut cfg = crate::PgConfig::new(isolated_url.clone()).with_vector_dim(4);
        cfg.table = "fastsearch_chunks_fs301_it".into();
        cfg.vector_type = crate::VectorType::Vector;
        let chunks = crate::PgStore::connect(cfg).await.expect("chunks connect");
        chunks.ensure_schema().await.expect("chunks schema");
        raw.batch_execute(&format!(
            "ALTER PUBLICATION fastsearch_pub ADD TABLE {jobs_table};"
        ))
        .await
        .expect("inject publication misconfiguration");
        chunks
            .ensure_schema()
            .await
            .expect("chunks-only publication reconciliation");
        let publication_tables: Vec<String> = raw
            .query(
                "SELECT c.relname FROM pg_publication_rel pr \
                 JOIN pg_publication p ON p.oid = pr.prpubid \
                 JOIN pg_class c ON c.oid = pr.prrelid \
                 WHERE p.pubname = 'fastsearch_pub' ORDER BY c.relname",
                &[],
            )
            .await
            .expect("publication query")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(publication_tables, vec!["fastsearch_chunks_fs301_it"]);

        drop(chunks);
        drop(one);
        drop(two);
        drop(three);
        drop(raw);
        admin
            .batch_execute(&format!("DROP DATABASE {database} WITH (FORCE);"))
            .await
            .expect("drop private database");
    }
}
