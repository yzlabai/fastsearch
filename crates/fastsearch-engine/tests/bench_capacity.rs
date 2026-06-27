//! 容量/性能基准（C2 数据源）。默认 `#[ignore]`，不进 CI（耗时 + 机器相关）。
//! 跑：`cargo test -p fastsearch-engine --test bench_capacity --release -- --ignored --nocapture`
//! 把打印的 BENCH 行填进 [容量与 SLO 文档](../../../docs/governance/2026-06-26-容量与SLO.md)。

use std::time::Instant;

use fastsearch_core::{BBox, Chunk, ChunkKind, SearchMode, SearchRequest};
use fastsearch_engine::{Engine, HnswParams, VectorBackendKind};
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
        tenant: None,
        acl: vec!["public".into()],
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
