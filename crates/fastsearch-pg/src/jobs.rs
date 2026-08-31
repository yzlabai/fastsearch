use crate::error::{PgError, Result};
use crate::{sql, validate_identifier, SCHEMA_DDL_LOCK_KEY};
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

    pub(crate) const fn as_str(self) -> &'static str {
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

/// Dedicated PostgreSQL connection for ingestion scheduling and fenced state mutations.
///
/// It is deliberately separate from [`crate::PgStore`]. A worker can create one `JobStore` per
/// concurrency slot, so claim transactions never serialize through the search source-of-truth
/// connection.
pub struct JobStore {
    client: Mutex<Client>,
    table: String,
}

impl JobStore {
    pub async fn connect(url: &str, table: impl Into<String>) -> Result<Self> {
        let table = table.into();
        validate_job_identifiers(&table)?;
        let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("fastsearch-pg job connection error: {error}");
            }
        });
        Ok(Self {
            client: Mutex::new(client),
            table,
        })
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// Additive, idempotent schema creation serialized with the existing chunks DDL lock.
    pub async fn ensure_schema(&self) -> Result<()> {
        let mut batch = String::from("BEGIN;\n");
        batch.push_str(&format!(
            "SELECT pg_advisory_xact_lock({SCHEMA_DDL_LOCK_KEY});\n"
        ));
        for statement in sql::job_ddl(&self.table) {
            batch.push_str(&statement);
            batch.push('\n');
        }
        batch.push_str("COMMIT;\n");
        self.client.lock().await.batch_execute(&batch).await?;
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

    pub async fn get(&self, job_id: &str) -> Result<Option<IngestJob>> {
        let row = self
            .client
            .lock()
            .await
            .query_opt(&sql::get_job_sql(&self.table), &[&job_id])
            .await?;
        row.as_ref().map(row_to_job).transpose()
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

fn checked_duration_ms(value: u64) -> Result<i64> {
    if value == 0 {
        return Err(PgError::Config("lease duration must be positive".into()));
    }
    i64::try_from(value).map_err(|_| PgError::Config("lease duration is too large".into()))
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
