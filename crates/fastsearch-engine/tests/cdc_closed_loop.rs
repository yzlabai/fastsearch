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
use fastsearch_sync::replication::{drop_slot, ensure_slot, pull_changes, ReplicationConfig};
use fastsearch_sync::{Applier, Lsn};
use fastsearch_text::TextIndexConfig;
use std::sync::OnceLock;

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
        image_meta: None,
        tenant: None,
        acl: vec!["public".into()],
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
    };

    // 0) 清理：重置 slot/publication/表，保证与运行顺序无关。
    reset(&url, slot).await;

    // 1) 真源 schema（建表 + publication FOR TABLE fastsearch_chunks）。
    let mut store = PgStore::connect(PgConfig::new(url.clone()))
        .await
        .expect("pg connect");
    store.ensure_schema().await.expect("ensure_schema");

    // 2) 先建 slot（之后的写入才会被捕获）。
    ensure_slot(&rcfg).await.expect("ensure_slot");

    // 3) 写 PG（真源）：doc 级替换写 3 个 chunk。
    let chunks = vec![
        chunk("rep.pdf", 1, "gross margin improved this year"),
        chunk("rep.pdf", 2, "revenue grew by eighteen percent"),
        chunk("rep.pdf", 3, "chip research investment increased"),
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
        ..Default::default()
    };
    let hits = engine.search(&req, None).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id.doc_id, "rep.pdf");
    assert_eq!(hits[0].id.chunk_id, 2);
    assert_eq!(hits[0].citation.page, 2);
    assert_eq!(hits[0].citation.heading_path, vec!["chapter", "sec"]);

    // 另一个词验证多 chunk 都进了索引
    let req2 = SearchRequest {
        query: "chip".into(),
        mode: SearchMode::Keyword,
        top_k: 5,
        ..Default::default()
    };
    assert_eq!(engine.search(&req2, None).unwrap()[0].id.chunk_id, 3);

    // 7) 清理 slot（避免 WAL 滞留）。
    drop_slot(&rcfg).await.expect("drop_slot");
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
    };
    reset(&url, slot).await;

    let mut store = PgStore::connect(PgConfig::new(url.clone()))
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
    };
    reset(&url, slot).await;

    let mut store = PgStore::connect(PgConfig::new(url.clone()))
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
    };
    reset(&url, slot).await;

    let mut store = PgStore::connect(PgConfig::new(url.clone()))
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
