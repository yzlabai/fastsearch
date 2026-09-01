use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use fastsearch_core::{BBox, Chunk, ChunkKind, Metadata};
use fastsearch_engine::Engine;
use fastsearch_pg::{IngestState, JobStore, NewIngestJob, PgConfig, PgStore};
use fastsearch_server::{router, Principal, ServerState};
use fastsearch_text::TextIndexConfig;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tower::ServiceExt;

fn chunk(doc_id: &str, text: &str) -> Chunk {
    Chunk {
        doc_id: doc_id.into(),
        chunk_id: 1,
        kind: ChunkKind::Paragraph,
        text: text.into(),
        page: 1,
        bbox: BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        },
        heading_path: Vec::new(),
        section_id: 0,
        char_len: text.len() as u32,
        media: None,
        media_bytes: None,
        image_vector_status: None,
        tenant: Some("acme".into()),
        acl: vec!["team-a".into()],
        metadata: Metadata::default(),
        searchable: true,
    }
}

fn request(path: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("x-api-key", key)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes(),
    )
    .expect("json")
}

#[tokio::test(flavor = "multi_thread")]
async fn t20c_failed_derived_publication_reclaims_same_job_and_converges_both_indexes() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skip t20c_failed_derived_publication_reclaims_same_job_and_converges_both_indexes: DATABASE_URL not set"
        );
        return;
    };
    let suffix = std::process::id();
    let jobs_table = format!("fs304_t20c_jobs_{suffix}");
    let chunks_table = format!("fs304_t20c_chunks_{suffix}");
    let (admin, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("admin connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    admin
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {jobs_table} CASCADE; \
             DROP TABLE IF EXISTS {chunks_table}_signal CASCADE; \
             DROP TABLE IF EXISTS {chunks_table} CASCADE;"
        ))
        .await
        .expect("clean schemas");

    let jobs = Arc::new(
        JobStore::connect_with_chunks_table(&url, &jobs_table, &chunks_table)
            .await
            .expect("jobs connect"),
    );
    let mut pg_config = PgConfig::new(url.clone()).with_vector_dim(8);
    pg_config.table = chunks_table.clone();
    let source = Arc::new(PgStore::connect(pg_config).await.expect("source connect"));
    source.ensure_schema().await.expect("source schema");
    jobs.ensure_schema().await.expect("jobs schema");
    jobs.submit_upload(&NewIngestJob {
        job_id: "t20c-job".into(),
        collection: "kb".into(),
        doc_id: "t20c.md".into(),
        tenant: Some("acme".into()),
        acl: vec!["team-a".into()],
        source_uri: "local://documents/acme/kb/t20c.md".into(),
        source_ready: true,
        content_sha256: "a".repeat(64),
        content_bytes: 32,
        media_type: Some("text/markdown".into()),
        filename: Some("t20c.md".into()),
        parse_profile: json!({}),
        max_retries: 3,
    })
    .await
    .expect("submit job");

    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine
        .ingest_vector("seed", &chunk("seed.md", "dimension seed"), vec![0.0; 8])
        .expect("seed vector dimension");
    engine.commit().expect("commit seed");
    engine.set_source_store(source.clone());
    let principal = Principal {
        tenant: Some("acme".into()),
        tags: vec!["team-a".into()],
    };
    let app = router(
        ServerState::new(
            engine,
            HashMap::from([
                ("owner-secret".into(), principal.clone()),
                ("worker-secret".into(), principal),
            ]),
        )
        .with_job_store(jobs.clone())
        .with_worker_keys(HashSet::from(["worker-secret".into()])),
    );

    let first = jobs
        .claim("worker-before-crash", 1, 10_000)
        .await
        .expect("claim")
        .pop()
        .expect("lease");
    let status = app
        .clone()
        .oneshot(request(
            "/v1/jobs/t20c-job/status",
            "worker-secret",
            json!({
                "lease_job_id":"t20c-job", "lease_owner":first.owner,
                "lease_epoch":first.job.lease_epoch, "state":"chunking"
            }),
        ))
        .await
        .expect("chunking response");
    assert_eq!(status.status(), StatusCode::OK);

    // A two-dimensional vector reaches the PG source write, then fails at the already-eight-
    // dimensional derived index. This is the production worker route, not a direct finish call.
    let failed = app
        .clone()
        .oneshot(request(
            "/v1/jobs/t20c-job/chunks",
            "worker-secret",
            json!({
                "lease_job_id":"t20c-job", "lease_owner":first.owner,
                "lease_epoch":first.job.lease_epoch,
                "chunks":[{
                    "chunk_id":1, "kind":"paragraph", "text":"t20c convergence marker",
                    "page":1, "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},
                    "heading_path":[], "section_id":0, "char_len":22,
                    "metadata":{}, "searchable":true, "vector":[1.0,0.0]
                }]
            }),
        ))
        .await
        .expect("failed publication response");
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        jobs.get("t20c-job").await.unwrap().unwrap().state,
        IngestState::Embedding,
        "derived failure must not publish indexed"
    );
    assert_eq!(
        source.fetch_doc("kb", "t20c.md").await.unwrap().len(),
        1,
        "PG truth committed before the derived failure"
    );

    let failed_status = app
        .clone()
        .oneshot(request(
            "/v1/jobs/t20c-job/status",
            "worker-secret",
            json!({
                "lease_job_id":"t20c-job", "lease_owner":first.owner,
                "lease_epoch":first.job.lease_epoch, "state":"failed",
                "error":"derived dimension mismatch", "error_stage":"embedding",
                "next_attempt_at_ms":0, "retryable":true
            }),
        ))
        .await
        .expect("record retryable failure");
    assert_eq!(failed_status.status(), StatusCode::OK);

    let second = jobs
        .claim("worker-after-crash", 1, 10_000)
        .await
        .expect("reclaim")
        .pop()
        .expect("reclaimed lease");
    assert_eq!(second.job.job_id, first.job.job_id);
    assert_eq!(second.job.lease_epoch, first.job.lease_epoch + 1);
    assert_eq!(
        app.clone()
            .oneshot(request(
                "/v1/jobs/t20c-job/status",
                "worker-secret",
                json!({
                    "lease_job_id":"t20c-job", "lease_owner":second.owner,
                    "lease_epoch":second.job.lease_epoch, "state":"chunking"
                }),
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let vector = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let published = app
        .clone()
        .oneshot(request(
            "/v1/jobs/t20c-job/chunks",
            "worker-secret",
            json!({
                "lease_job_id":"t20c-job", "lease_owner":second.owner,
                "lease_epoch":second.job.lease_epoch,
                "chunks":[{
                    "chunk_id":1, "kind":"paragraph", "text":"t20c convergence marker",
                    "page":1, "bbox":{"x0":0.0,"y0":0.0,"x1":1.0,"y1":1.0},
                    "heading_path":[], "section_id":0, "char_len":22,
                    "metadata":{}, "searchable":true, "vector":vector
                }]
            }),
        ))
        .await
        .expect("publish retry");
    assert_eq!(published.status(), StatusCode::OK);
    assert_eq!(
        jobs.get("t20c-job").await.unwrap().unwrap().state,
        IngestState::Indexed
    );

    for search in [
        json!({"query":"t20c convergence", "mode":"keyword", "top_k":5}),
        json!({"query":"", "mode":"vector", "vector":vector, "top_k":5}),
    ] {
        let response = app
            .clone()
            .oneshot(request("/v1/search", "owner-secret", search))
            .await
            .expect("search response");
        assert_eq!(response.status(), StatusCode::OK);
        let response = body_json(response).await;
        assert!(response["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["doc_id"] == "t20c.md"));
    }

    admin
        .batch_execute(&format!(
            "DROP TABLE {jobs_table} CASCADE; DROP TABLE {chunks_table}_signal CASCADE; \
             DROP TABLE {chunks_table} CASCADE;"
        ))
        .await
        .expect("cleanup");
}
