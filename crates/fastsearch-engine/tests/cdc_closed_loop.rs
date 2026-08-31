//! CDC 端到端闭环（env-gated，需活 PG，`wal_level=logical`）：
//!
//!   写 PG（真源 PgStore）→ 逻辑复制 slot（pgoutput）→ pull_changes 解码 →
//!   Applier 应用到 Engine（IndexSink）→ 检索命中
//!
//! 未设 `DATABASE_URL` 则跳过（不算失败）。本测试自清理（重置 slot/publication/表），
//! 与运行顺序无关。

use fastsearch_core::{BBox, Chunk, ChunkKind, SearchMode, SearchRequest};
use fastsearch_engine::Engine;
use fastsearch_pg::{PgConfig, PgStore};
use fastsearch_sync::replication::{
    drop_slot, ensure_slot, peek_batch, pull_changes, ReplicationConfig,
};
use fastsearch_sync::{Applier, Change, ChangeEvent, Lsn};
use fastsearch_text::TextIndexConfig;
use std::sync::OnceLock;

struct DelayedEmbedder {
    inner: fastsearch_embed::HashEmbedder,
    started: std::sync::Arc<tokio::sync::Notify>,
    delay: std::time::Duration,
}

impl fastsearch_embed::Embedder for DelayedEmbedder {
    fn dim(&self) -> usize {
        fastsearch_embed::Embedder::dim(&self.inner)
    }

    fn embed(
        &self,
        texts: &[String],
        kind: fastsearch_embed::EmbedKind,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        fastsearch_embed::Embedder::embed(&self.inner, texts, kind)
    }

    fn embed_multi(
        &self,
        inputs: &[fastsearch_embed::EmbedInput],
        kind: fastsearch_embed::EmbedKind,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        self.started.notify_one();
        std::thread::sleep(self.delay);
        fastsearch_embed::Embedder::embed_multi(&self.inner, inputs, kind)
    }
}

/// 两个集成测试共享同名 publication/表，必须串行（否则并发 reset 互相踩）。
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
async fn serial_guard() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn chunk(doc: &str, id: u64, text: &str) -> Chunk {
    Chunk {
        doc_id: doc.into(),
        chunk_id: id,
        kind: ChunkKind::Paragraph,
        text: text.into(),
        page: id as u32,
        bbox: BBox {
            x0: 1.0,
            y0: 2.0,
            x1: 3.0,
            y1: 4.0,
        },
        heading_path: vec!["chapter".into(), "sec".into()],
        section_id: 7,
        char_len: text.len() as u32,
        media: None,
        media_bytes: None,
        image_vector_status: None,
        tenant: None,
        acl: vec!["public".into()],
        metadata: Default::default(),
        searchable: true,
    }
}

/// 直连跑建表前的清理 SQL（重置共享对象，保证幂等/隔离）。
async fn reset(url: &str, slot: &str) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect for reset");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    // slot 先删（drop 表/publication 不影响 slot）。
    let _ = client
        .execute(
            "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE slot_name=$1",
            &[&slot],
        )
        .await;
    let _ = client
        .batch_execute(
            "DROP PUBLICATION IF EXISTS fastsearch_pub; DROP TABLE IF EXISTS fastsearch_chunks;",
        )
        .await;
}

#[tokio::test]
async fn cdc_closed_loop_pg_to_search() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_closed_loop_pg_to_search: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };

    // 0) 清理：重置 slot/publication/表，保证与运行顺序无关。
    reset(&url, slot).await;

    // 1) 真源 schema（建表 + publication FOR TABLE fastsearch_chunks）。
    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");

    // 2) 先建 slot（之后的写入才会被捕获）。
    ensure_slot(&rcfg).await.expect("ensure_slot");

    // 3) 写 PG（真源）：doc 级替换写 3 个 chunk。
    let mut revenue = chunk("rep.pdf", 2, "revenue grew by eighteen percent");
    revenue
        .metadata
        .insert("source".into(), serde_json::json!("ledger"));
    let mut hidden = chunk("rep.pdf", 3, "chip research investment increased");
    hidden.searchable = false;
    let chunks = vec![
        chunk("rep.pdf", 1, "gross margin improved this year"),
        revenue,
        hidden,
    ];
    let n = store
        .upsert_doc("kb", "rep.pdf", &chunks)
        .await
        .expect("upsert_doc");
    assert_eq!(n, 3);

    // 4) CDC：从 slot 拉取并解码变更（应为 3 条 Upsert）。
    let events = pull_changes(&rcfg).await.expect("pull_changes");
    let upserts = events.len();
    assert_eq!(upserts, 3, "expected 3 upsert events, got {events:?}");

    // 5) 应用到 Engine（IndexSink）+ 提交。
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    let mut applier = Applier::new(Lsn(0));
    let applied = applier
        .apply_batch(&mut engine, &events)
        .expect("apply_batch");
    assert_eq!(applied, 3);

    // 6) 检索命中（闭环验证：PG 写的内容能在引擎检索到，带正确引用）。
    let req = SearchRequest {
        query: "revenue".into(),
        mode: SearchMode::Keyword,
        top_k: 5,
        include_metadata: true,
        ..Default::default()
    };
    let hits = engine.search(&req, None).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id.doc_id, "rep.pdf");
    assert_eq!(hits[0].id.chunk_id, 2);
    assert_eq!(hits[0].citation.page, 2);
    assert_eq!(hits[0].citation.heading_path, vec!["chapter", "sec"]);
    assert_eq!(hits[0].metadata.as_ref().unwrap()["source"], "ledger");

    // searchable=false 的真源行会经过 CDC，但不会进入派生检索索引。
    let req2 = SearchRequest {
        query: "chip".into(),
        mode: SearchMode::Keyword,
        top_k: 5,
        ..Default::default()
    };
    assert!(engine.search(&req2, None).unwrap().is_empty());

    // 7) 清理 slot（避免 WAL 滞留）。
    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-101：一条 PK UPDATE 必须按同一 WAL/LSN 有序执行 Delete(old)+Upsert(new)；
/// TRUNCATE 必须清空 text/vector 两套派生索引，三种搜索模式均不能返回幽灵命中。
#[tokio::test]
async fn cdc_pk_update_and_truncate_converge() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_pk_update_and_truncate_converge: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_pk_truncate_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc(
            "kb",
            "old.pdf",
            &[chunk("old.pdf", 1, "migrationmarker source text")],
        )
        .await
        .expect("insert source row");

    let embed_cfg = fastsearch_embed::EmbedderConfig::hash(8);
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine.set_embedder(fastsearch_embed::build_embedder(&embed_cfg));
    let mut applier = Applier::new(Lsn(0));
    let inserted = pull_changes(&rcfg).await.expect("pull insert");
    applier
        .apply_batch(&mut engine, &inserted)
        .expect("apply insert");

    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("raw connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .execute(
            "UPDATE fastsearch_chunks \
             SET collection='kb2', doc_id='new.pdf', chunk_id=2 \
             WHERE collection='kb' AND doc_id='old.pdf' AND chunk_id=1",
            &[],
        )
        .await
        .expect("update primary key");

    let moved = pull_changes(&rcfg).await.expect("pull PK update");
    assert_eq!(moved.len(), 1, "一条 WAL UPDATE 应保持为一个复合事件");
    assert!(matches!(moved[0].change, fastsearch_sync::Change::Batch(_)));
    applier
        .apply_batch(&mut engine, &moved)
        .expect("apply PK update");

    let hits = engine
        .search(
            &SearchRequest {
                query: "migrationmarker".into(),
                mode: SearchMode::Keyword,
                top_k: 5,
                ..Default::default()
            },
            None,
        )
        .expect("keyword after PK update");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id.to_citation_id(), "kb2:new.pdf:2");

    let query_vector = fastsearch_embed::Embedder::embed(
        &*fastsearch_embed::build_embedder(&embed_cfg),
        &["migrationmarker source text".to_string()],
        fastsearch_embed::EmbedKind::Query,
    )
    .expect("embed query")
    .remove(0);

    client
        .batch_execute("TRUNCATE fastsearch_chunks")
        .await
        .expect("truncate source table");
    let truncated = pull_changes(&rcfg).await.expect("pull truncate");
    assert_eq!(truncated.len(), 1);
    assert!(matches!(
        truncated[0].change,
        fastsearch_sync::Change::Clear
    ));
    applier
        .apply_batch(&mut engine, &truncated)
        .expect("apply truncate");

    for mode in [SearchMode::Keyword, SearchMode::Vector, SearchMode::Hybrid] {
        let hits = engine
            .search(
                &SearchRequest {
                    query: "migrationmarker".into(),
                    mode,
                    vector: (mode != SearchMode::Keyword).then(|| query_vector.clone()),
                    top_k: 5,
                    ..Default::default()
                },
                None,
            )
            .expect("search after truncate");
        assert!(hits.is_empty(), "{mode:?} 仍返回 TRUNCATE 前幽灵命中");
    }

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-102：pgvector 写穿必须是整批事务。第二条 UPDATE 注入失败时，第一条 embedding 也不得
/// 对向量检索可见；解除故障后重试同一批，keyword/vector 应共同收敛。
#[tokio::test(flavor = "multi_thread")]
async fn cdc_pg_write_failure_rolls_back_and_retry_converges() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_pg_write_failure_rolls_back_and_retry_converges: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_pg_failure_test";
    reset(&url, slot).await;

    let store = std::sync::Arc::new(
        PgStore::connect(PgConfig::new(url.clone()).with_vector_dim(8))
            .await
            .expect("pg connect"),
    );
    store.ensure_schema().await.expect("ensure_schema");
    store
        .upsert_doc(
            "kb",
            "failure.pdf",
            &[
                chunk("failure.pdf", 1, "pg-failure-marker common one"),
                chunk("failure.pdf", 2, "pg-failure-marker common two"),
            ],
        )
        .await
        .expect("insert source rows");

    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("raw connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE OR REPLACE FUNCTION fs102_reject_second_embedding() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
               IF NEW.chunk_id = 2 AND NEW.embedding IS NOT NULL THEN \
                 RAISE EXCEPTION 'injected pg write-through failure'; \
               END IF; \
               RETURN NEW; \
             END $$; \
             CREATE TRIGGER fs102_reject_second_embedding \
             BEFORE UPDATE OF embedding ON fastsearch_chunks \
             FOR EACH ROW EXECUTE FUNCTION fs102_reject_second_embedding();",
        )
        .await
        .expect("install failure trigger");

    let embed_cfg = fastsearch_embed::EmbedderConfig::hash(8);
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine.set_embedder(fastsearch_embed::build_embedder(&embed_cfg));
    engine.set_pg_vector(store);
    engine.set_embed_model("fs102-test@8");
    let events: Vec<_> = (1..=2)
        .map(|id| ChangeEvent {
            change: Change::Upsert {
                collection: "kb".into(),
                chunk: Box::new(chunk(
                    "failure.pdf",
                    id,
                    &format!("pg-failure-marker common {id}"),
                )),
            },
            lsn: Lsn(id),
        })
        .collect();
    let mut applier = Applier::new(Lsn(0));
    let err = applier.apply_batch(&mut engine, &events).unwrap_err();
    assert!(
        err.to_string().contains("pg embedding batch"),
        "unexpected error: {err:#}"
    );
    assert_eq!(applier.applied_lsn(), Lsn(0));
    engine.commit().expect("unrelated commit after failure");

    let keyword = engine
        .search(
            &SearchRequest {
                query: "pg-failure-marker".into(),
                mode: SearchMode::Keyword,
                top_k: 5,
                ..Default::default()
            },
            None,
        )
        .expect("keyword after failure");
    assert!(keyword.is_empty());
    let query_vector = fastsearch_embed::Embedder::embed(
        &*fastsearch_embed::build_embedder(&embed_cfg),
        &["pg-failure-marker common one".into()],
        fastsearch_embed::EmbedKind::Passage,
    )
    .expect("embed query")
    .remove(0);
    let vector_request = SearchRequest {
        query: String::new(),
        mode: SearchMode::Vector,
        vector: Some(query_vector),
        top_k: 5,
        ..Default::default()
    };
    assert!(
        engine
            .search(&vector_request, None)
            .expect("vector after failure")
            .is_empty(),
        "失败事务的第一条 embedding 不得残留"
    );

    client
        .batch_execute(
            "DROP TRIGGER fs102_reject_second_embedding ON fastsearch_chunks; \
             DROP FUNCTION fs102_reject_second_embedding();",
        )
        .await
        .expect("remove failure trigger");
    assert_eq!(applier.apply_batch(&mut engine, &events).unwrap(), 2);
    assert_eq!(applier.applied_lsn(), Lsn(2));
    assert_eq!(
        engine
            .search(
                &SearchRequest {
                    query: "pg-failure-marker".into(),
                    mode: SearchMode::Keyword,
                    top_k: 5,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .len(),
        2
    );
    assert_eq!(engine.search(&vector_request, None).unwrap().len(), 2);

    reset(&url, slot).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cdc_failed_write_through_marks_recovery_until_replay_completes() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skip cdc_failed_write_through_marks_recovery_until_replay_completes: DATABASE_URL not set"
        );
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_recovery_marker_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = std::sync::Arc::new(
        PgStore::connect(PgConfig::new(url.clone()).with_vector_dim(8))
            .await
            .expect("pg connect"),
    );
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc(
            "kb",
            "recovery.pdf",
            &[
                chunk("recovery.pdf", 1, "recovery marker one"),
                chunk("recovery.pdf", 2, "recovery marker two"),
            ],
        )
        .await
        .expect("insert source rows");

    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("raw connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE OR REPLACE FUNCTION fs103_reject_second_embedding() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
               IF NEW.chunk_id = 2 AND NEW.embedding IS NOT NULL THEN \
                 RAISE EXCEPTION 'injected pg write-through failure'; \
               END IF; \
               RETURN NEW; \
             END $$; \
             CREATE TRIGGER fs103_reject_second_embedding \
             BEFORE UPDATE OF embedding ON fastsearch_chunks \
             FOR EACH ROW EXECUTE FUNCTION fs103_reject_second_embedding();",
        )
        .await
        .expect("install failure trigger");

    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine.set_embedder(fastsearch_embed::build_embedder(
        &fastsearch_embed::EmbedderConfig::hash(8),
    ));
    engine.set_pg_vector(store);
    engine.set_embed_model("fs103-test@8");
    let engine = std::sync::Arc::new(tokio::sync::Mutex::new(engine));
    let data = tempfile::tempdir().expect("tempdir");

    let error = Engine::consume_once_shared(&engine, &rcfg, data.path())
        .await
        .expect_err("write-through must fail");
    assert!(error.to_string().contains("pg write-through"));
    assert!(Engine::cdc_recovery_pending(data.path()).unwrap());

    client
        .batch_execute(
            "DROP TRIGGER fs103_reject_second_embedding ON fastsearch_chunks; \
             DROP FUNCTION fs103_reject_second_embedding();",
        )
        .await
        .expect("remove failure trigger");
    let stats = Engine::consume_once_shared(&engine, &rcfg, data.path())
        .await
        .expect("replay consume");
    assert_eq!(stats.applied, 2);
    assert!(!Engine::cdc_recovery_pending(data.path()).unwrap());

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-102：外部嵌入等待期间，搜索仍应能取得 Engine 锁；只有本地发布/持久化短暂持锁。
#[tokio::test(flavor = "multi_thread")]
async fn cdc_batch_embedding_does_not_hold_engine_lock() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_batch_embedding_does_not_hold_engine_lock: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_lock_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;
    let mut pg_cfg = PgConfig::new(url.clone());
    pg_cfg.vector_dim = 8;
    let store = std::sync::Arc::new(PgStore::connect(pg_cfg).await.expect("pg connect"));
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc(
            "kb",
            "lock.pdf",
            &[chunk("lock.pdf", 1, "lock-free-embedding-marker")],
        )
        .await
        .expect("insert source row");

    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine.set_embedder(Box::new(DelayedEmbedder {
        inner: fastsearch_embed::HashEmbedder::new(8),
        started: started.clone(),
        delay: std::time::Duration::from_millis(200),
    }));
    engine.set_pg_vector(store.clone());
    let engine = std::sync::Arc::new(tokio::sync::Mutex::new(engine));
    let data = tempfile::tempdir().expect("tempdir");
    let consume = tokio::spawn({
        let engine = engine.clone();
        let rcfg = rcfg.clone();
        let data = data.path().to_path_buf();
        async move { Engine::consume_once_shared(&engine, &rcfg, &data).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .expect("embedding did not start");

    let locked = tokio::time::timeout(std::time::Duration::from_millis(75), engine.lock())
        .await
        .expect("Engine lock was held during external embedding");
    assert!(
        locked
            .search(
                &SearchRequest {
                    query: "lock-free-embedding-marker".into(),
                    mode: SearchMode::Keyword,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .is_empty(),
        "prepared batch must not publish before lock-protected apply"
    );
    let query_vector = fastsearch_embed::Embedder::embed(
        &fastsearch_embed::HashEmbedder::new(8),
        &["lock-free-embedding-marker".into()],
        fastsearch_embed::EmbedKind::Passage,
    )
    .unwrap()
    .remove(0);
    // 继续占住 Engine 锁直到嵌入已经完成：旧实现会在锁外先提交 PG 写穿，导致这里
    // 能看到新向量、上面的 keyword 仍为空。新实现必须等取得同一把锁后才写穿并发布。
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert!(
        locked
            .search(
                &SearchRequest {
                    query: String::new(),
                    mode: SearchMode::Vector,
                    vector: Some(query_vector.clone()),
                    top_k: 5,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .is_empty(),
        "PG vector must not publish ahead of the lock-protected keyword batch"
    );
    drop(locked);

    let stats = consume.await.unwrap().unwrap();
    assert_eq!(stats.applied, 1);
    assert!(stats.prepare_micros >= 150_000);
    // PG embedding 写穿本身可能产生一拍 publication 反馈；幂等 guard 会令下一拍稳定为空。
    let feedback_stats = Engine::consume_once_shared(&engine, &rcfg, data.path())
        .await
        .expect("write-through feedback poll");
    let idle_stats = Engine::consume_once_shared(&engine, &rcfg, data.path())
        .await
        .expect("idle CDC poll");
    assert_eq!(idle_stats.applied, 0, "CDC feedback must converge");
    assert_eq!(
        idle_stats.last_applied_lsn, feedback_stats.last_applied_lsn,
        "idle polls must retain the persisted last-applied commit LSN"
    );
    assert!(
        engine
            .lock()
            .await
            .search(
                &SearchRequest {
                    query: "lock-free-embedding-marker".into(),
                    mode: SearchMode::Keyword,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .len()
            == 1
    );
    assert_eq!(
        engine
            .lock()
            .await
            .search(
                &SearchRequest {
                    query: String::new(),
                    mode: SearchMode::Vector,
                    vector: Some(query_vector),
                    top_k: 5,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .len(),
        1
    );
    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-102：模拟进程在本地 apply 后、persist/slot advance 前退出。未持久化批次不得留下单路新版本；
/// 重启后 slot 重放同批，keyword/vector 一起收敛。
#[tokio::test(flavor = "multi_thread")]
async fn cdc_crash_after_apply_before_persist_retries_without_half_state() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skip cdc_crash_after_apply_before_persist_retries_without_half_state: DATABASE_URL not set"
        );
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_crash_before_persist_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;
    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc(
            "kb",
            "crash.pdf",
            &[chunk("crash.pdf", 1, "crash-retry-marker")],
        )
        .await
        .expect("insert source row");

    let data = tempfile::tempdir().expect("tempdir");
    let embed_cfg = fastsearch_embed::EmbedderConfig::hash(8);
    {
        let (mut engine, _) =
            Engine::open(data.path(), TextIndexConfig::default()).expect("open engine");
        engine.set_embedder(fastsearch_embed::build_embedder(&embed_cfg));
        let (events, _) = fastsearch_sync::replication::peek_with_lsn(&rcfg)
            .await
            .expect("peek");
        let prepared = engine
            .cdc_batch_preparer()
            .prepare(events.into_iter().map(|event| event.change).collect())
            .await
            .expect("prepare");
        engine
            .apply_prepared_cdc_batch(prepared)
            .expect("apply prepared");
        // 故障注入：此处直接 drop，刻意不 persist、不 advance slot。
    }

    let (mut restarted, lsn) =
        Engine::open(data.path(), TextIndexConfig::default()).expect("restart after crash");
    assert_eq!(lsn, Lsn(0));
    assert!(restarted
        .search(
            &SearchRequest {
                query: "crash-retry-marker".into(),
                mode: SearchMode::Keyword,
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .is_empty());
    let query_vector = fastsearch_embed::Embedder::embed(
        &*fastsearch_embed::build_embedder(&embed_cfg),
        &["crash-retry-marker".into()],
        fastsearch_embed::EmbedKind::Passage,
    )
    .unwrap()
    .remove(0);
    let vector_request = SearchRequest {
        query: String::new(),
        mode: SearchMode::Vector,
        vector: Some(query_vector),
        top_k: 5,
        ..Default::default()
    };
    assert!(restarted.search(&vector_request, None).unwrap().is_empty());

    restarted.set_embedder(fastsearch_embed::build_embedder(&embed_cfg));
    assert_eq!(
        restarted
            .consume_once(&rcfg, data.path())
            .await
            .expect("retry consume"),
        1
    );
    assert_eq!(
        restarted
            .search(
                &SearchRequest {
                    query: "crash-retry-marker".into(),
                    mode: SearchMode::Keyword,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .len(),
        1
    );
    assert_eq!(restarted.search(&vector_request, None).unwrap().len(), 1);
    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-103：补齐 peek / persist / advance 三个崩溃边界；apply 边界由上一个测试覆盖。
/// 每个边界重启后都只能得到一个最终 GlobalId，且 slot 最终无残留。
#[tokio::test(flavor = "multi_thread")]
async fn cdc_crash_at_peek_persist_and_advance_recovers_without_loss_or_duplicates() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skip cdc_crash_at_peek_persist_and_advance_recovers_without_loss_or_duplicates: DATABASE_URL not set"
        );
        return;
    };
    let _guard = serial_guard().await;

    for (phase, expected_replay) in [("peek", 1), ("persist", 1), ("advance", 0)] {
        let slot = format!("fastsearch_cdc_crash_{phase}_test");
        let rcfg = ReplicationConfig {
            url: url.clone(),
            slot: slot.clone(),
            publication: "fastsearch_pub".into(),
            source_table: "public.fastsearch_chunks".into(),
        };
        reset(&url, &slot).await;
        let store = PgStore::connect(PgConfig::new(url.clone()))
            .await
            .expect("pg connect");
        store.ensure_schema().await.expect("ensure_schema");
        ensure_slot(&rcfg).await.expect("ensure_slot");
        let marker = format!("crash-{phase}-marker");
        store
            .upsert_doc(
                "kb",
                &format!("{phase}.pdf"),
                &[chunk(&format!("{phase}.pdf"), 1, &marker)],
            )
            .await
            .expect("insert source row");

        let data = tempfile::tempdir().expect("tempdir");
        {
            let (mut engine, _) =
                Engine::open(data.path(), TextIndexConfig::default()).expect("open engine");
            let batch = peek_batch(&rcfg).await.expect("peek crash-point batch");
            assert_eq!(batch.events.len(), 1);
            if phase != "peek" {
                let prepared = engine
                    .cdc_batch_preparer()
                    .prepare(batch.events.into_iter().map(|event| event.change).collect())
                    .await
                    .expect("prepare");
                engine
                    .apply_prepared_cdc_batch(prepared)
                    .expect("apply prepared");
                engine
                    .persist(data.path(), batch.commit_lsn)
                    .expect("persist");
                if phase == "advance" {
                    fastsearch_sync::replication::advance_slot(&rcfg, batch.commit_lsn)
                        .await
                        .expect("advance");
                }
            }
            // 故障注入：分别在 peek、persist 或 advance 后直接 drop。
        }

        let (mut restarted, _) =
            Engine::open(data.path(), TextIndexConfig::default()).expect("restart engine");
        let replayed = restarted
            .consume_once(&rcfg, data.path())
            .await
            .expect("recovery consume");
        assert_eq!(replayed, expected_replay, "unexpected replay at {phase}");
        let hits = restarted
            .search(
                &SearchRequest {
                    query: marker,
                    mode: SearchMode::Keyword,
                    top_k: 5,
                    ..Default::default()
                },
                None,
            )
            .expect("search after recovery");
        assert_eq!(hits.len(), 1, "loss or duplicate after {phase} crash");
        assert!(fastsearch_sync::replication::peek_changes(&rcfg)
            .await
            .expect("peek after recovery")
            .is_empty());
        drop_slot(&rcfg).await.expect("drop_slot");
    }
}

/// 崩溃安全的 CDC 消费 + 派生索引持久化（env-gated：仅需 PG；用 Hash 嵌入→离线确定性）：
///
///   peek（不推进 slot）→ 应用（apply_upsert 含嵌入）→ persist（索引+检查点落盘）→
///   落盘后 advance_slot；重启从检查点续传、向量不重嵌、不丢不重。
#[tokio::test]
async fn cdc_consume_persist_crashsafe() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_consume_persist_crashsafe: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_persist_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc(
            "kb",
            "rep.pdf",
            &[chunk("rep.pdf", 1, "alpha"), chunk("rep.pdf", 2, "beta")],
        )
        .await
        .expect("upsert_doc");

    let data = tempfile::tempdir().unwrap();
    let cfg = TextIndexConfig::default();
    // Hash 嵌入：离线、确定性（不需 Ollama），8 维。
    let hash_cfg = fastsearch_embed::EmbedderConfig::hash(8);
    {
        let (mut e, lsn0) = Engine::open(data.path(), cfg).expect("open");
        assert_eq!(lsn0, Lsn(0)); // 首启无检查点
        e.set_embedder(fastsearch_embed::build_embedder(&hash_cfg));
        let n = e
            .consume_once(&rcfg, data.path())
            .await
            .expect("consume_once");
        assert_eq!(n, 2);
    } // drop engine（释放 Tantivy 写锁，模拟重启）

    // slot 已推进：再 peek 应为空（不重发已确认的变更）。
    let again = fastsearch_sync::replication::peek_changes(&rcfg)
        .await
        .expect("peek");
    assert!(
        again.is_empty(),
        "advanced slot should yield no changes, got {again:?}"
    );

    // 重开：检查点续传（applied_lsn=slot 高水位>0）+ 向量在（无需重嵌）。
    let (e2, lsn) = Engine::open(data.path(), TextIndexConfig::default()).expect("reopen");
    assert!(
        lsn > Lsn(0),
        "applied_lsn 应从 checkpoint 恢复为 slot 高水位"
    );
    // 向量路：两 chunk 都已嵌入落盘 → vector 检索两条都在。
    let qv = fastsearch_embed::Embedder::embed(
        &*fastsearch_embed::build_embedder(&hash_cfg),
        &["alpha".to_string()],
        fastsearch_embed::EmbedKind::Query,
    )
    .unwrap()
    .remove(0);
    let mut r = SearchRequest {
        query: String::new(),
        mode: SearchMode::Vector,
        vector: Some(qv),
        top_k: 5,
        ..Default::default()
    };
    r.candidates = 150;
    let hits = e2.search(&r, None).expect("vector search");
    assert_eq!(hits.len(), 2, "两 chunk 向量都应已持久化");
    assert!(hits.iter().all(|h| h.vector.is_some()));

    // 幂等：无新变更（slot 已 advance），consume_once 返回 0（peek 空）。
    let mut e3 = e2;
    e3.set_embedder(fastsearch_embed::build_embedder(&hash_cfg));
    let n2 = e3
        .consume_once(&rcfg, data.path())
        .await
        .expect("consume_once again");
    assert_eq!(n2, 0);

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// 初始快照 bootstrap + 无缝衔接（env-gated：仅需 PG）：先写**存量**→建 slot 取一致点→
/// fetch_all→bootstrap_snapshot→检索命中存量；再写增量→consume_once→共可检索（不丢/不重）。
#[tokio::test]
async fn cdc_initial_snapshot_bootstrap() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_initial_snapshot_bootstrap: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_bootstrap_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");

    // **先写存量**（在建 slot 之前）——这正是 bootstrap 要解决的：slot 之前的数据。
    store
        .upsert_doc(
            "kb",
            "rep.pdf",
            &[
                chunk("rep.pdf", 1, "alpha existing"),
                chunk("rep.pdf", 2, "beta existing"),
            ],
        )
        .await
        .expect("upsert existing");

    // 建 slot → 取一致点 LSN（新建返回 Some）。
    let consistent = ensure_slot(&rcfg).await.expect("ensure_slot");
    assert!(
        consistent.is_some(),
        "newly created slot should return consistent lsn"
    );
    let consistent = consistent.unwrap();
    assert!(consistent > Lsn(0));
    // 再调一次 → None（已存在，幂等）。
    assert!(ensure_slot(&rcfg).await.expect("ensure_slot2").is_none());

    // 全表读 → bootstrap 进引擎（Hash 嵌入，离线确定性）。
    let rows = store.fetch_all_chunks().await.expect("fetch_all");
    assert_eq!(rows.len(), 2);
    let data = tempfile::tempdir().unwrap();
    let (mut engine, lsn0) = Engine::open(data.path(), TextIndexConfig::default()).expect("open");
    assert_eq!(lsn0, Lsn(0));
    engine.set_embedder(fastsearch_embed::build_embedder(
        &fastsearch_embed::EmbedderConfig::hash(8),
    ));
    let imported = engine
        .bootstrap_snapshot(&rows, data.path(), consistent)
        .expect("bootstrap");
    assert_eq!(imported, 2);

    // 存量可检索（keyword）。
    let hits = engine
        .search(
            &SearchRequest {
                query: "existing".into(),
                mode: SearchMode::Keyword,
                top_k: 5,
                ..Default::default()
            },
            None,
        )
        .expect("search");
    assert_eq!(hits.len(), 2, "bootstrap 应导入两条存量");

    // 无缝衔接：bootstrap 后再写增量 → consume_once 拉到 1 条 → 共 3 条。
    store
        .upsert_doc(
            "kb",
            "more.pdf",
            &[chunk("more.pdf", 1, "gamma incremental")],
        )
        .await
        .expect("upsert incremental");
    let n = engine
        .consume_once(&rcfg, data.path())
        .await
        .expect("consume_once");
    assert_eq!(n, 1, "增量应只看到 bootstrap 之后的 1 条");
    let all = engine
        .search(
            &SearchRequest {
                query: "existing incremental".into(),
                mode: SearchMode::Keyword,
                top_k: 10,
                ..Default::default()
            },
            None,
        )
        .expect("search all");
    assert_eq!(all.len(), 3, "存量 2 + 增量 1 = 3，均可检索（不丢/不重）");

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// FS-103：多副本并发首建同一 slot 必须全部成功；恰好一个实例拿到一致点，其余识别为已存在。
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cdc_concurrent_slot_creation_is_idempotent() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_concurrent_slot_creation_is_idempotent: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_concurrent_slot_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let cfg = rcfg.clone();
        tasks.push(tokio::spawn(async move { ensure_slot(&cfg).await }));
    }
    let mut created = 0;
    for task in tasks {
        if task
            .await
            .expect("ensure_slot task")
            .expect("ensure_slot")
            .is_some()
        {
            created += 1;
        }
    }
    assert_eq!(created, 1, "exactly one replica creates the slot");
    assert!(ensure_slot(&rcfg).await.unwrap().is_none());
    drop_slot(&rcfg).await.expect("drop_slot");
}

#[tokio::test]
async fn cdc_peek_exposes_commit_lsn_lag_and_dead_letters() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_peek_exposes_commit_lsn_lag_and_dead_letters: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_status_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");
    store
        .upsert_doc("kb", "status.pdf", &[chunk("status.pdf", 1, "cdc status")])
        .await
        .expect("upsert_doc");

    let batch = peek_batch(&rcfg).await.expect("peek_batch");
    assert_eq!(batch.events.len(), 1);
    assert!(batch.commit_lsn > Lsn(0));
    assert!(batch.slot_lag_bytes > 0);
    assert_eq!(batch.dead_letters, 0);

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// 完整产品主循环（双 env-gated：需 PG + 本地 Ollama）：
///
///   写 PG → 逻辑复制 → pgoutput 解码 → **CDC 落地时自动嵌入** → 派生 BM25+向量 →
///   语义查询（词面不重叠）走 vector 命中
///
/// 设 `DATABASE_URL` 与 `FASTSEARCH_EMBED_TEST_URL` 才跑；缺任一则跳过。
#[tokio::test]
async fn cdc_embed_hybrid_full_loop() {
    let (Ok(url), Ok(emb_url)) = (
        std::env::var("DATABASE_URL"),
        std::env::var("FASTSEARCH_EMBED_TEST_URL"),
    ) else {
        eprintln!("skip cdc_embed_hybrid_full_loop: need DATABASE_URL + FASTSEARCH_EMBED_TEST_URL");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_embed_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");
    ensure_slot(&rcfg).await.expect("ensure_slot");

    // 写 PG：语义可区分两段（盈利能力 vs 停车）。
    let mut a = chunk("rep.pdf", 1, "本季度公司盈利能力显著改善，净利润增长。");
    a.heading_path = vec!["财务".into()];
    let mut b = chunk("rep.pdf", 2, "新办公楼的访客停车位安排与门禁说明。");
    b.heading_path = vec!["行政".into()];
    store
        .upsert_doc("kb", "rep.pdf", &[a, b])
        .await
        .expect("upsert_doc");

    // CDC 拉取。
    let events = pull_changes(&rcfg).await.expect("pull_changes");
    assert_eq!(events.len(), 2);

    // 引擎 + Ollama 嵌入后端：apply_upsert 会自动嵌入 → 写向量索引。
    let mut ecfg = fastsearch_embed::EmbedderConfig::from_env();
    ecfg.url = emb_url;
    if !matches!(ecfg.kind, fastsearch_embed::EmbedderKind::Http(_)) {
        ecfg.kind = fastsearch_embed::EmbedderKind::Http(fastsearch_embed::HttpProtocol::Ollama);
    }
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    engine.set_embedder(fastsearch_embed::build_embedder(&ecfg));
    let mut applier = Applier::new(Lsn(0));
    applier
        .apply_batch(&mut engine, &events)
        .expect("apply_batch");

    // 语义查询（与 chunk1 词面几乎不重叠）：先嵌入 query，再走 vector 模式。
    let qv = fastsearch_embed::Embedder::embed(
        &*fastsearch_embed::build_embedder(&ecfg),
        &["企业的赚钱能力如何".to_string()],
        fastsearch_embed::EmbedKind::Query,
    )
    .expect("embed query")
    .remove(0);
    let req = SearchRequest {
        query: "企业的赚钱能力如何".into(),
        mode: SearchMode::Vector,
        vector: Some(qv),
        top_k: 5,
        ..Default::default()
    };
    let hits = engine.search(&req, None).expect("search");
    assert!(!hits.is_empty(), "vector search returned no hits");
    assert_eq!(
        hits[0].id.chunk_id, 1,
        "semantically closest chunk should rank first"
    );
    assert!(hits[0].vector.is_some(), "vector score present");

    drop_slot(&rcfg).await.expect("drop_slot");
}

/// H3 回归（env-gated：需 PG）：TOAST 大列在 UPDATE 里未变 → pgoutput 发 'u'(UnchangedToast)。
/// 旧代码把 'u' 当 missing/null → `row_to_chunk` 报错 → 整批失败 → slot 永不 advance（毒丸卡死）。
/// 修复后 map 检测 'u' → 从 PG 真源**重取整行**，pull_changes 不再报错，大 text 不丢。
#[tokio::test]
async fn cdc_unchanged_toast_update_does_not_stall() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skip cdc_unchanged_toast_update_does_not_stall: DATABASE_URL not set");
        return;
    };
    let _guard = serial_guard().await;
    let slot = "fastsearch_cdc_toast_test";
    let rcfg = ReplicationConfig {
        url: url.clone(),
        slot: slot.into(),
        publication: "fastsearch_pub".into(),
        source_table: "public.fastsearch_chunks".into(),
    };
    reset(&url, slot).await;

    let store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");

    // 直连：把 text 列存储改 EXTERNAL（强制大值出行外存、不压缩）→ 未改 text 的 UPDATE 必发 'u'。
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("raw connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
        .batch_execute("ALTER TABLE fastsearch_chunks ALTER COLUMN text SET STORAGE EXTERNAL")
        .await
        .expect("set storage external");

    ensure_slot(&rcfg).await.expect("ensure_slot");

    // 大 text（>8KB，出行外存 TOAST），含检索词 "toasted"。
    let big = format!("toasted marker {}", "abcdefgh".repeat(1500));
    store
        .upsert_doc("kb", "big.pdf", &[chunk("big.pdf", 1, &big)])
        .await
        .expect("upsert big");
    let ev1 = pull_changes(&rcfg).await.expect("pull insert");
    assert!(!ev1.is_empty(), "insert 应产生事件");

    // 原地 UPDATE 只改小列 tenant（不动大 text）→ pgoutput 对 text 发 'u'（UnchangedToast）。
    client
        .execute(
            "UPDATE fastsearch_chunks SET tenant='acme' \
             WHERE collection='kb' AND doc_id='big.pdf' AND chunk_id=1",
            &[],
        )
        .await
        .expect("update small col");

    // 关键：pull_changes 不再因 'u' 报错（毒丸），而是从真源重取整行 → 一条有效 Upsert。
    let ev2 = pull_changes(&rcfg)
        .await
        .expect("pull_changes 不应因 UnchangedToast 卡死(毒丸)");
    assert_eq!(
        ev2.len(),
        1,
        "UnchangedToast UPDATE 应重取为 1 条 upsert, got {ev2:?}"
    );

    // 应用后：大 text 仍可检索（重取保住了 text，未丢）。
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).expect("engine");
    let mut applier = Applier::new(Lsn(0));
    applier
        .apply_batch(&mut engine, &ev1)
        .expect("apply insert");
    applier
        .apply_batch(&mut engine, &ev2)
        .expect("apply update");
    let req = SearchRequest {
        query: "toasted".into(),
        mode: SearchMode::Keyword,
        top_k: 5,
        ..Default::default()
    };
    let hits = engine.search(&req, None).expect("search");
    assert_eq!(hits.len(), 1, "大 text 应仍可检索（重取保住了 text）");
    assert_eq!(hits[0].id.chunk_id, 1);

    drop_slot(&rcfg).await.expect("drop_slot");
}
