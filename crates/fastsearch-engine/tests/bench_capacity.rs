//! 容量/性能基准（C2 数据源）。默认 `#[ignore]`，不进 CI（耗时 + 机器相关）。
//! 跑：`cargo test -p fastsearch-engine --test bench_capacity --release -- --ignored --nocapture`
//! 把打印的 BENCH 行填进 [容量与 SLO 文档](../../../docs/governance/2026-06-26-容量与SLO.md)。

use std::time::Instant;

use fastsearch_core::{BBox, Chunk, ChunkKind, SearchMode, SearchRequest};
use fastsearch_embed::{EmbedInput, EmbedKind, Embedder, HashEmbedder};
use fastsearch_engine::{Engine, HnswParams, VectorBackendKind};
use fastsearch_sync::{Change, ChangeEvent, IndexSink, Lsn};
use fastsearch_text::TextIndexConfig;

const DIM: usize = 96;
const N: usize = 10_000;
const QUERIES: usize = 200;
const K: usize = 10;

// 确定性伪随机（线性同余），避免依赖 rand + 保证可复现。
fn rng(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed
        .wrapping_mul(2862933555777941757)
        .wrapping_add(3037000493);
    move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s
    }
}

fn vec_for(seed: u64) -> Vec<f32> {
    let mut r = rng(seed);
    (0..DIM)
        .map(|_| ((r() >> 33) as f32 / (1u64 << 31) as f32) - 1.0)
        .collect()
}

const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon",
];

fn text_for(seed: u64) -> String {
    let mut r = rng(seed ^ 0x9e3779b9);
    (0..12)
        .map(|_| WORDS[(r() as usize) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn chunk(id: u64, text: String) -> Chunk {
    Chunk {
        doc_id: format!("doc{}", id / 50),
        chunk_id: id,
        kind: ChunkKind::Paragraph,
        text,
        page: (id % 100) as u32,
        bbox: BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        },
        heading_path: vec![],
        section_id: 0,
        char_len: 60,
        media: None,
        media_bytes: None,
        image_vector_status: None,
        tenant: None,
        acl: vec!["public".into()],
        metadata: Default::default(),
        searchable: true,
    }
}

fn percentile(mut xs: Vec<f64>, p: f64) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((xs.len() as f64 - 1.0) * p).round() as usize;
    xs[i]
}

fn build(backend: VectorBackendKind) -> (Engine, std::time::Duration) {
    let mut e = Engine::create_in_ram_with(TextIndexConfig::default(), backend).unwrap();
    let t = Instant::now();
    for i in 0..N as u64 {
        e.ingest_vector("kb", &chunk(i, text_for(i)), vec_for(i))
            .unwrap();
    }
    e.commit().unwrap();
    (e, t.elapsed())
}

fn vec_req(v: Vec<f32>) -> SearchRequest {
    SearchRequest {
        query: String::new(),
        mode: SearchMode::Vector,
        vector: Some(v),
        top_k: K,
        ..Default::default()
    }
}

fn kw_req(q: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        mode: SearchMode::Keyword,
        top_k: K,
        ..Default::default()
    }
}

fn latency_ms(e: &Engine, reqs: &[SearchRequest]) -> (f64, f64) {
    let mut lat = Vec::with_capacity(reqs.len());
    for r in reqs {
        let t = Instant::now();
        let _ = e.search(r, None).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    (percentile(lat.clone(), 0.50), percentile(lat, 0.95))
}

#[ignore]
#[test]
fn bench_capacity() {
    println!("BENCH config N={N} dim={DIM} queries={QUERIES} k={K}");

    // 暴力档
    let (brute, brute_ingest) = build(VectorBackendKind::Brute);
    let kw: Vec<_> = (0..QUERIES)
        .map(|i| kw_req(WORDS[i % WORDS.len()]))
        .collect();
    let vq: Vec<_> = (0..QUERIES)
        .map(|i| vec_req(vec_for(1_000_000 + i as u64)))
        .collect();
    let (kw_p50, kw_p95) = latency_ms(&brute, &kw);
    let (bv_p50, bv_p95) = latency_ms(&brute, &vq);
    println!(
        "BENCH brute ingest={:.2}s ({:.0} chunks/s) kw_p50={kw_p50:.3}ms kw_p95={kw_p95:.3}ms vec_p50={bv_p50:.3}ms vec_p95={bv_p95:.3}ms",
        brute_ingest.as_secs_f64(),
        N as f64 / brute_ingest.as_secs_f64()
    );

    // HNSW 档
    let (hnsw, hnsw_ingest) = build(VectorBackendKind::Hnsw(HnswParams::default()));
    let (hv_p50, hv_p95) = latency_ms(&hnsw, &vq);
    println!(
        "BENCH hnsw  ingest={:.2}s ({:.0} chunks/s) vec_p50={hv_p50:.3}ms vec_p95={hv_p95:.3}ms",
        hnsw_ingest.as_secs_f64(),
        N as f64 / hnsw_ingest.as_secs_f64()
    );

    // HNSW recall@k vs 暴力 ground-truth
    let mut hit = 0usize;
    for r in &vq {
        let truth: std::collections::HashSet<_> = brute
            .search(r, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        let got = hnsw.search(r, None).unwrap();
        hit += got.iter().filter(|h| truth.contains(&h.id)).count();
    }
    println!(
        "BENCH hnsw recall@{K}={:.3} (vs brute)",
        hit as f64 / (K * QUERIES) as f64
    );
}

struct LatencyEmbedder {
    inner: HashEmbedder,
    per_request: std::time::Duration,
}

impl Embedder for LatencyEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        self.inner.embed(texts, kind)
    }

    fn embed_multi(&self, inputs: &[EmbedInput], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        std::thread::sleep(self.per_request);
        self.inner.embed_multi(inputs, kind)
    }
}

fn cdc_engine(delay: std::time::Duration) -> Engine {
    let mut engine = Engine::create_in_ram(TextIndexConfig::default()).unwrap();
    engine.set_embedder(Box::new(LatencyEmbedder {
        inner: HashEmbedder::new(DIM),
        per_request: delay,
    }));
    engine
}

/// FS-102 的 before/after 数据源：legacy 逐 chunk 外部调用 vs CDC 整批 prepare。
/// 默认 ignore，机器相关结果只作同机对拍，不设跨机器硬阈值。
#[ignore]
#[test]
fn bench_cdc_batch() {
    const CDC_N: usize = 64;
    const SEARCH_PROBES: usize = 64;
    let delay = std::time::Duration::from_millis(5);
    let chunks: Vec<_> = (0..CDC_N as u64)
        .map(|id| chunk(id, format!("cdc batch marker {}", text_for(id))))
        .collect();

    let events: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| ChangeEvent {
            change: Change::Upsert {
                collection: "kb".into(),
                chunk: Box::new(chunk.clone()),
            },
            lsn: Lsn(index as u64 + 1),
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let (
        legacy_elapsed,
        legacy_search_p95,
        batch_elapsed,
        batch_search_p95,
        prepare_elapsed,
        batch_lock_wait,
        batch_lock_hold,
    ) = runtime.block_on(async {
        let legacy = std::sync::Arc::new(tokio::sync::Mutex::new(cdc_engine(delay)));
        let legacy_writer = legacy.clone();
        let legacy_chunks = chunks.clone();
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let legacy_started = Instant::now();
        let legacy_task = tokio::spawn(async move {
            let mut engine = legacy_writer.lock().await;
            let _ = locked_tx.send(());
            for chunk in &legacy_chunks {
                engine.apply_upsert("kb", chunk).unwrap();
            }
            engine.commit().unwrap();
        });
        locked_rx.await.unwrap();
        let mut legacy_searches = Vec::with_capacity(SEARCH_PROBES);
        for _ in 0..SEARCH_PROBES {
            let engine = legacy.clone();
            legacy_searches.push(tokio::spawn(async move {
                let started = Instant::now();
                let locked = engine.lock().await;
                locked.search(&kw_req("marker"), None).unwrap();
                started.elapsed().as_secs_f64() * 1000.0
            }));
        }
        legacy_task.await.unwrap();
        let legacy_elapsed = legacy_started.elapsed();
        let mut legacy_latencies = Vec::with_capacity(SEARCH_PROBES);
        for search in legacy_searches {
            legacy_latencies.push(search.await.unwrap());
        }

        let batched = std::sync::Arc::new(tokio::sync::Mutex::new(cdc_engine(delay)));
        let preparer = batched.lock().await.cdc_batch_preparer();
        let changes: Vec<Change> = events.iter().map(|event| event.change.clone()).collect();
        let batch_started = Instant::now();
        let prepare_started = Instant::now();
        let prepare_task = tokio::spawn(async move { preparer.prepare(changes).await });
        tokio::task::yield_now().await;
        let mut batch_searches = Vec::with_capacity(SEARCH_PROBES);
        for _ in 0..SEARCH_PROBES {
            let engine = batched.clone();
            batch_searches.push(tokio::spawn(async move {
                let started = Instant::now();
                let locked = engine.lock().await;
                locked.search(&kw_req("marker"), None).unwrap();
                started.elapsed().as_secs_f64() * 1000.0
            }));
        }
        let prepared = prepare_task.await.unwrap().unwrap();
        let prepare_elapsed = prepare_started.elapsed();
        let lock_started = Instant::now();
        let mut locked = batched.lock().await;
        let batch_lock_wait = lock_started.elapsed();
        let hold_started = Instant::now();
        locked.apply_prepared_cdc_batch(prepared).unwrap();
        locked.commit().unwrap();
        let batch_lock_hold = hold_started.elapsed();
        drop(locked);
        let batch_elapsed = batch_started.elapsed();
        let mut batch_latencies = Vec::with_capacity(SEARCH_PROBES);
        for search in batch_searches {
            batch_latencies.push(search.await.unwrap());
        }

        (
            legacy_elapsed,
            percentile(legacy_latencies, 0.95),
            batch_elapsed,
            percentile(batch_latencies, 0.95),
            prepare_elapsed,
            batch_lock_wait,
            batch_lock_hold,
        )
    });

    println!(
        "BENCH cdc N={CDC_N} legacy={:.2} chunks/s batch={:.2} chunks/s speedup={:.2}x legacy_search_p95={legacy_search_p95:.3}ms batch_search_p95={batch_search_p95:.3}ms prepare={:.2}ms batch_lock_wait={:.3}ms batch_lock_hold={:.2}ms",
        CDC_N as f64 / legacy_elapsed.as_secs_f64(),
        CDC_N as f64 / batch_elapsed.as_secs_f64(),
        legacy_elapsed.as_secs_f64() / batch_elapsed.as_secs_f64(),
        prepare_elapsed.as_secs_f64() * 1000.0,
        batch_lock_wait.as_secs_f64() * 1000.0,
        batch_lock_hold.as_secs_f64() * 1000.0,
    );
}
