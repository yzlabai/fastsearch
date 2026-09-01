use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy)]
pub(super) enum IngestEvent {
    Upload,
    Deduplicated,
    Failure { retryable: bool },
    ManualRetry,
    SyncHit,
    SyncTimeout,
}

/// Ingestion telemetry is a deep module: handlers report domain events and the module owns
/// counter relationships plus their Prometheus representation.
#[derive(Default)]
pub(super) struct IngestTelemetry {
    uploads: AtomicU64,
    deduplicated: AtomicU64,
    failures: AtomicU64,
    retryable_failures: AtomicU64,
    terminal_failures: AtomicU64,
    manual_retries: AtomicU64,
    sync_hits: AtomicU64,
    sync_timeouts: AtomicU64,
}

impl IngestTelemetry {
    pub(super) fn record(&self, event: IngestEvent) {
        let counter = match event {
            IngestEvent::Upload => &self.uploads,
            IngestEvent::Deduplicated => &self.deduplicated,
            IngestEvent::Failure { retryable } => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                if retryable {
                    &self.retryable_failures
                } else {
                    &self.terminal_failures
                }
            }
            IngestEvent::ManualRetry => &self.manual_retries,
            IngestEvent::SyncHit => &self.sync_hits,
            IngestEvent::SyncTimeout => &self.sync_timeouts,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn render(&self, out: &mut String) {
        counter(
            out,
            "fastsearch_ingest_uploads_total",
            "Document upload submissions resolved by the job store.",
            self.uploads.load(Ordering::Relaxed),
        );
        counter(
            out,
            "fastsearch_ingest_deduplicated_total",
            "Document uploads deduplicated or coalesced with existing work.",
            self.deduplicated.load(Ordering::Relaxed),
        );
        counter(
            out,
            "fastsearch_ingest_failures_total",
            "Worker failures accepted by the fenced job state machine.",
            self.failures.load(Ordering::Relaxed),
        );
        out.push_str(
            "# HELP fastsearch_ingest_failures_classified_total Worker failures by durable classification.\n\
             # TYPE fastsearch_ingest_failures_classified_total counter\n",
        );
        out.push_str(&format!(
            "fastsearch_ingest_failures_classified_total{{classification=\"retryable\"}} {}\n\
             fastsearch_ingest_failures_classified_total{{classification=\"terminal\"}} {}\n",
            self.retryable_failures.load(Ordering::Relaxed),
            self.terminal_failures.load(Ordering::Relaxed),
        ));
        counter(
            out,
            "fastsearch_ingest_manual_retries_total",
            "Dead-letter ingest jobs manually requeued by their owner.",
            self.manual_retries.load(Ordering::Relaxed),
        );
        counter(
            out,
            "fastsearch_ingest_sync_hit_total",
            "Synchronous upload waits that observed an indexed terminal state.",
            self.sync_hits.load(Ordering::Relaxed),
        );
        counter(
            out,
            "fastsearch_ingest_sync_timeout_total",
            "Synchronous upload waits that degraded to asynchronous polling.",
            self.sync_timeouts.load(Ordering::Relaxed),
        );
    }

    pub(super) fn render_snapshot(out: &mut String, ingest: &fastsearch_pg::IngestMetrics) {
        out.push_str(
            "# HELP fastsearch_ingest_jobs_total Current ingest jobs by authoritative state.\n\
             # TYPE fastsearch_ingest_jobs_total gauge\n",
        );
        for (state, count) in &ingest.state_counts {
            out.push_str(&format!(
                "fastsearch_ingest_jobs_total{{state=\"{}\"}} {count}\n",
                state.as_str()
            ));
        }
        out.push_str(&format!(
            "# HELP fastsearch_ingest_dead_letter_total Current failed ingest jobs whose retry budget is exhausted.\n\
             # TYPE fastsearch_ingest_dead_letter_total gauge\n\
             fastsearch_ingest_dead_letter_total {}\n",
            ingest.dead_letter_count.max(0)
        ));
        for (name, help, value) in [
            (
                "fastsearch_ingest_retryable_failed",
                "Current failed ingest jobs classified as retryable.",
                ingest.retryable_failed_count,
            ),
            (
                "fastsearch_ingest_jobs_source_pending",
                "Current ingest jobs waiting for their reserved raw source.",
                ingest.source_pending_count,
            ),
            (
                "fastsearch_ingest_jobs_cleanup_pending",
                "Current ingest jobs retaining a superseded raw object cleanup hint.",
                ingest.cleanup_pending_count,
            ),
            (
                "fastsearch_ingest_leases_active",
                "Current unexpired ingest worker leases.",
                ingest.active_lease_count,
            ),
            (
                "fastsearch_ingest_leases_expired",
                "Current expired ingest worker leases awaiting reclaim.",
                ingest.expired_lease_count,
            ),
            (
                "fastsearch_ingest_workers_seen_recently",
                "Distinct ingest workers seen in the last 120 seconds.",
                ingest.workers_seen_recently,
            ),
            (
                "fastsearch_ingest_oldest_ready_age_seconds",
                "Age since the oldest ready job became claimable.",
                ingest.oldest_ready_age_seconds,
            ),
        ] {
            gauge(out, name, help, value.max(0) as u64);
        }
    }
}

pub(super) fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

pub(super) fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
    ));
}
