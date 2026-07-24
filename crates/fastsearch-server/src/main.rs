//! fastsearch-server 二进制：起 REST 服务（四张脸之一）。
//!
//! 配置（环境变量）：
//! - `FASTSEARCH_DATA`：索引数据目录（默认 `./data`）。
//! - `FASTSEARCH_PORT`：监听端口（默认 8642）。
//! - `FASTSEARCH_KEYS`：API Key 表，格式 `key=tenant:tag1,tag2;key2=:public`
//!   （tenant 留空=管理员/无租户限制）。未设则建一个 dev key `dev`（无租户限制）。
//! - `FASTSEARCH_RATE_LIMIT`：`capacity,refill_per_sec`（每 key 令牌桶）；未设=不限流。
//! - `FASTSEARCH_AUDIT`：设为 `1`/`stderr` 则每个成功请求向 stderr 输出一行审计 JSON。
//! - `FASTSEARCH_S3_ENDPOINT` / `_REGION` / `_BUCKET` / `_ACCESS_KEY` / `_SECRET_KEY`：
//!   真实 S3/MinIO 对象存储（SigV4）。设了 endpoint 时优先使用。
//! - `FASTSEARCH_OBJECT_DIR`：本地对象存储根目录（S3-compatible URI 的 bucket/key 映射到此目录）。
//! - `FASTSEARCH_OBJECT_BUCKET`：本地对象存储默认 bucket（默认 `fastsearch-assets`）。
//! - `FASTSEARCH_S3_MAX_IMAGE_BYTES`：对象读写最大字节数（默认 20MiB）。
//! - `FASTSEARCH_EMBEDDER` = `hash`|`ollama`|`openai`（+ `FASTSEARCH_EMBED_*`）：真语义嵌入后端。
//! - `FASTSEARCH_VECTOR_BACKEND` = `brute`(默认)|`brute_binary`|`brute_binary_rotated`|`turboquant`|`hnsw`|`pgvector`：向量后端。
//!   `turboquant`=压缩主索引（只存 2–4bit 码、内存 ↓8~16×、确定，位宽由 `FASTSEARCH_QUANT_BITS` 调，默认 4；
//!   `FASTSEARCH_TURBO_RERANK=<oversample>` 开 f32 精排 sidecar → 召回近精确、RAM 仍只码）；hnsw=引擎侧近似
//!   ANN（大规模、近似+非确定，仅首启生效）；pgvector=直查档（ANN 在 PG 跑，需 `DATABASE_URL` +
//!   embedding 已入 PG，引擎写穿为下一迭代）。
//! - `FASTSEARCH_CDC=1`（+ `DATABASE_URL`，可选 `FASTSEARCH_CDC_SLOT`/`_PUBLICATION`/`_INTERVAL_MS`）：
//!   起后台 CDC 同步循环（崩溃安全、落盘续传），从 PG 真源把变更同步到派生索引。

use fastsearch_engine::Engine;
use fastsearch_server::{router, AuditSink, Principal, ServerState};
use fastsearch_text::{TextIndexConfig, TokenizerKind};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn parse_keys(spec: &str) -> HashMap<String, Principal> {
    let mut keys = HashMap::new();
    for entry in spec.split(';').filter(|s| !s.trim().is_empty()) {
        let (key, rest) = match entry.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let (tenant, tags) = rest.split_once(':').unwrap_or((rest, ""));
        let tenant = if tenant.trim().is_empty() {
            None
        } else {
            Some(tenant.trim().to_string())
        };
        let tags = tags
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        keys.insert(key.trim().to_string(), Principal { tenant, tags });
    }
    keys
}

/// 读取浮点环境变量；缺失或不可解析则回退 `default`（带 warn）。
fn env_f32(key: &str, default: f32) -> f32 {
    match std::env::var(key) {
        Ok(s) => s.trim().parse().unwrap_or_else(|_| {
            eprintln!("warn: {key}={s:?} 非法浮点，回退 {default}");
            default
        }),
        Err(_) => default,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data: PathBuf = std::env::var("FASTSEARCH_DATA")
        .unwrap_or_else(|_| "./data".into())
        .into();
    let port: u16 = std::env::var("FASTSEARCH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8642);
    let keys = match std::env::var("FASTSEARCH_KEYS") {
        Ok(spec) => parse_keys(&spec),
        Err(_) => {
            let mut m = HashMap::new();
            m.insert(
                "dev".to_string(),
                Principal {
                    tenant: None,
                    tags: vec![],
                },
            );
            eprintln!("FASTSEARCH_KEYS not set; using dev key 'dev' (no tenant restriction)");
            m
        }
    };

    // 默认 jieba（面向 docparse 中文为主的语料）；FASTSEARCH_TOKENIZER=default 可切。
    let tokenizer = match std::env::var("FASTSEARCH_TOKENIZER").as_deref() {
        Ok("default") => TokenizerKind::Default,
        _ => TokenizerKind::Jieba,
    };
    // BM25 k1/b（词频饱和 / 长度归一）：偏离 Tantivy 默认(1.2/0.75)即触发引擎自算重排（A11）。
    // 仅首启建索引时定型（写进派生索引的 schema 语义无关，但换值需重建以重排既有 doc）。
    let default_cfg = TextIndexConfig::default();
    let k1 = env_f32("FASTSEARCH_BM25_K1", default_cfg.k1);
    let b = env_f32("FASTSEARCH_BM25_B", default_cfg.b);
    let cfg = TextIndexConfig {
        tokenizer,
        k1,
        b,
        ..default_cfg
    };
    // 向量后端：FASTSEARCH_VECTOR_BACKEND=hnsw 用 HNSW 近似（大规模，近似+非确定）；
    // `brute_binary` 暴力 + 二值量化粗筛（大集合更快、仍确定，oversample 由
    // FASTSEARCH_BINARY_OVERSAMPLE 调，默认 8）；`brute_binary_rotated` 再叠 RaBitQ 随机旋转
    // （召回更高、尤利各向异性嵌入）；默认 brute 暴力精确。仅首启（无检查点）生效；已建索引沿用记录的后端。
    let backend = match std::env::var("FASTSEARCH_VECTOR_BACKEND").as_deref() {
        Ok("hnsw") => {
            fastsearch_engine::VectorBackendKind::Hnsw(fastsearch_engine::HnswParams::default())
        }
        Ok(v @ ("brute_binary" | "brute_binary_rotated")) => {
            let m = std::env::var("FASTSEARCH_BINARY_OVERSAMPLE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(fastsearch_engine::DEFAULT_BINARY_OVERSAMPLE);
            if v == "brute_binary_rotated" {
                fastsearch_engine::VectorBackendKind::BruteBinaryRotated(m)
            } else {
                fastsearch_engine::VectorBackendKind::BruteBinary(m)
            }
        }
        // TurboQuant 压缩主索引（只存 2–4bit 码，内存 ↓8~16×、确定；位宽由 FASTSEARCH_QUANT_BITS 调，默认 4）。
        // FASTSEARCH_TURBO_RERANK=<oversample> 开 f32 精排 sidecar（磁盘 f32 精排，召回近精确、RAM 仍只码）。
        Ok("turboquant") => {
            let bits = std::env::var("FASTSEARCH_QUANT_BITS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(fastsearch_engine::DEFAULT_QUANT_BITS);
            let rerank_oversample = std::env::var("FASTSEARCH_TURBO_RERANK")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            fastsearch_engine::VectorBackendKind::TurboQuant {
                bits,
                rerank_oversample,
            }
        }
        _ => fastsearch_engine::VectorBackendKind::Brute,
    };
    // 打开数据目录下的派生索引（落盘恢复）：text + vector.bin + checkpoint.json。
    let (mut engine, start_lsn) = Engine::open_with(&data, cfg, backend)?;

    // 嵌入后端配置（FASTSEARCH_EMBEDDER=ollama|openai；默认 hash→不嵌入）。**提前读取**：PG 表的
    // `embedding halfvec(dim)` 列维度须与 embedder 输出维度一致（M18），故建 PgConfig 前先拿到 dim。
    let ecfg = fastsearch_embed::EmbedderConfig::from_env();
    let embed_dim = ecfg.dim;

    // pgvector 直查档（B6）：向量召回改在 PG 跑 ANN（需 DATABASE_URL）。引擎侧向量后端仍建
    // （Brute，空置不用）。注意：embedding 须已在 PG（外部嵌入管线写入；引擎写穿为下一迭代）。
    if matches!(
        std::env::var("FASTSEARCH_VECTOR_BACKEND").as_deref(),
        Ok("pgvector")
    ) {
        match std::env::var("DATABASE_URL") {
            Ok(url) => {
                let pg = fastsearch_pg::PgStore::connect(
                    fastsearch_pg::PgConfig::new(url).with_vector_dim(embed_dim),
                )
                .await?;
                pg.ensure_schema().await?;
                engine.set_pg_vector(std::sync::Arc::new(pg));
                eprintln!("vector backend: pgvector 直查（ANN 在 PG，需 embedding 已入 PG）");
            }
            Err(_) => eprintln!("FASTSEARCH_VECTOR_BACKEND=pgvector 但未设 DATABASE_URL，跳过"),
        }
    }

    // 媒资真源（MM6-inline）：媒资网关 `/v1/asset` 的 Inline 路径从 PG `media_bytes` 按需取字节。
    // 与向量后端无关——只要有 PG 真源（DATABASE_URL）即开启（字节是真源、引擎派生层不持）。
    if let Ok(url) = std::env::var("DATABASE_URL") {
        match fastsearch_pg::PgStore::connect(
            fastsearch_pg::PgConfig::new(url.clone()).with_vector_dim(embed_dim),
        )
        .await
        {
            Ok(pg) => {
                // 幂等建表 + vector 扩展 + publication（并发 boot 安全：advisory lock 串行化）
                if let Err(e) = pg.ensure_schema().await {
                    eprintln!("warn: ensure_schema failed: {e}（`/v1/index` 真源回写将失败）");
                }
                engine.set_source_store(std::sync::Arc::new(pg));
                eprintln!("media source: PG media_bytes（/v1/asset inline 字节）");
            }
            Err(e) => eprintln!("media source store 连接失败: {e}（inline 字节不可用）"),
        }
    }

    if let Ok(endpoint) = std::env::var("FASTSEARCH_S3_ENDPOINT") {
        let region = std::env::var("FASTSEARCH_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let bucket = std::env::var("FASTSEARCH_S3_BUCKET").map_err(|_| {
            anyhow::anyhow!("FASTSEARCH_S3_BUCKET is required when FASTSEARCH_S3_ENDPOINT is set")
        })?;
        let access_key = std::env::var("FASTSEARCH_S3_ACCESS_KEY").map_err(|_| {
            anyhow::anyhow!(
                "FASTSEARCH_S3_ACCESS_KEY is required when FASTSEARCH_S3_ENDPOINT is set"
            )
        })?;
        let secret_key = std::env::var("FASTSEARCH_S3_SECRET_KEY").map_err(|_| {
            anyhow::anyhow!(
                "FASTSEARCH_S3_SECRET_KEY is required when FASTSEARCH_S3_ENDPOINT is set"
            )
        })?;
        let max_bytes = std::env::var("FASTSEARCH_S3_MAX_IMAGE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20 * 1024 * 1024);
        let store =
            fastsearch_engine::S3ObjectStore::new(endpoint, region, bucket, access_key, secret_key)
                .with_max_bytes(max_bytes);
        engine.set_object_store(Arc::new(store));
        eprintln!("object store on: S3-compatible endpoint");
    } else if let Ok(root) = std::env::var("FASTSEARCH_OBJECT_DIR") {
        let bucket = std::env::var("FASTSEARCH_OBJECT_BUCKET")
            .unwrap_or_else(|_| "fastsearch-assets".into());
        let max_bytes = std::env::var("FASTSEARCH_S3_MAX_IMAGE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20 * 1024 * 1024);
        let store =
            fastsearch_engine::LocalObjectStore::new(root, bucket).with_max_bytes(max_bytes);
        engine.set_object_store(Arc::new(store));
        eprintln!("object store on: local S3-compatible directory");
    }

    let embed_on = matches!(ecfg.kind, fastsearch_embed::EmbedderKind::Http(_));
    if embed_on {
        // CDC 落地路径用引擎自身的 embedder 嵌入 passage。
        // 写穿标记 = "模型@维度"（落 PG embed_model，溯源 + 幂等守卫；换模型/换维度即变标记）。
        engine.set_embed_model(format!("{}@{}", ecfg.model, ecfg.dim));
        engine.set_embedder(fastsearch_embed::build_embedder(&ecfg));
    }

    // CDC 设置（在建 state 前——bootstrap 需 &mut engine）。slot 位置由 PG 服务端持久，
    // 故只需 (rcfg, interval)。返回 None 表示不开 CDC。
    let cdc: Option<(
        fastsearch_sync::replication::ReplicationConfig,
        std::time::Duration,
    )> = if matches!(std::env::var("FASTSEARCH_CDC").as_deref(), Ok("1")) {
        match std::env::var("DATABASE_URL") {
            Ok(url) => {
                let rcfg = fastsearch_sync::replication::ReplicationConfig {
                    url: url.clone(),
                    slot: std::env::var("FASTSEARCH_CDC_SLOT")
                        .unwrap_or_else(|_| "fastsearch_slot".into()),
                    publication: std::env::var("FASTSEARCH_CDC_PUBLICATION")
                        .unwrap_or_else(|_| "fastsearch_pub".into()),
                };
                let interval_ms: u64 = std::env::var("FASTSEARCH_CDC_INTERVAL_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1000);
                let created = fastsearch_sync::replication::ensure_slot(&rcfg).await?;
                // 首启（引擎无检查点）+ 新建 slot → 初始快照导入存量（从一致点）。
                if start_lsn == fastsearch_sync::Lsn(0) {
                    match created {
                        Some(consistent) => {
                            let store = fastsearch_pg::PgStore::connect(
                                fastsearch_pg::PgConfig::new(url).with_vector_dim(embed_dim),
                            )
                            .await?;
                            let rows = store.fetch_all_chunks().await?;
                            if !rows.is_empty() {
                                let n = engine.bootstrap_snapshot(&rows, &data, consistent)?;
                                eprintln!(
                                    "cdc bootstrap: imported {n} existing row(s) at {consistent:?}"
                                );
                            }
                        }
                        None => eprintln!(
                            "warning: slot 已存在但引擎无检查点，跳过快照、从 slot 现位增量"
                        ),
                    }
                }
                Some((rcfg, std::time::Duration::from_millis(interval_ms)))
            }
            Err(_) => {
                eprintln!("FASTSEARCH_CDC=1 但未设 DATABASE_URL，跳过 CDC");
                None
            }
        }
    } else {
        None
    };

    let mut state = ServerState::new(engine, keys).with_vector_dim(embed_dim);

    // 限流：FASTSEARCH_RATE_LIMIT="capacity,refill_per_sec"
    if let Ok(spec) = std::env::var("FASTSEARCH_RATE_LIMIT") {
        if let Some((cap, refill)) = spec.split_once(',') {
            if let (Ok(cap), Ok(refill)) = (cap.trim().parse(), refill.trim().parse()) {
                state = state.with_rate_limit(cap, refill);
                eprintln!("rate limit on: capacity={cap}, refill={refill}/s per key");
            }
        }
    }
    // 资产 URL 签名（MM6-signer）：FASTSEARCH_ASSET_SIGNING_KEY 设密钥即开启短时 token URL
    // （`/v1/assets/resolve` 签发、`/v1/asset/{cid}/bytes` 凭 token 取字节，让前端 <img src> 免 Bearer）。
    // FASTSEARCH_ASSET_URL_TTL 调过期秒数（默认 300）。多副本须同密钥。
    let signer_enabled = if let Ok(key) = std::env::var("FASTSEARCH_ASSET_SIGNING_KEY") {
        if !key.is_empty() {
            let ttl = std::env::var("FASTSEARCH_ASSET_URL_TTL")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300);
            state = state.with_asset_signer(key.into_bytes(), ttl);
            state = state.enable_object_url_signer().await;
            eprintln!("asset URL signing on (TTL {ttl}s)");
            true
        } else {
            false
        }
    } else {
        false
    };
    // 公网入口 base：`search`/`similar` 命中拼 `media.url` 时用。
    // 单独配置无效（签发需 signer）；仅设 base 时启动日志 warn，但不留 `media.url`。
    if let Ok(base) = std::env::var("FASTSEARCH_PUBLIC_URL") {
        let base = base.trim().to_string();
        if !base.is_empty() {
            state = state.with_public_base(base.clone());
            if !signer_enabled {
                eprintln!(
                    "warn: FASTSEARCH_PUBLIC_URL={base} 已设但 asset signer 未启用，\
                     `media.url` 不会出现在 search 命中"
                );
            } else {
                eprintln!("media url base: {base}");
            }
        }
    }
    // 审计：FASTSEARCH_AUDIT=1|stderr → stderr JSON 一行一事件
    if matches!(
        std::env::var("FASTSEARCH_AUDIT").as_deref(),
        Ok("1") | Ok("stderr")
    ) {
        let sink: AuditSink = Arc::new(|ev| {
            if let Ok(line) = serde_json::to_string(&ev) {
                eprintln!("{line}");
            }
        });
        state = state.with_audit(sink);
        eprintln!("audit log on (stderr JSON)");
    }
    // query 侧嵌入（与 CDC sink 的 engine.embedder 同配置、独立实例）。
    if embed_on {
        state = state.with_embedder(std::sync::Arc::from(fastsearch_embed::build_embedder(
            &ecfg,
        )));
        eprintln!(
            "embedder on: {:?} url={} model={} dim={}",
            ecfg.kind, ecfg.url, ecfg.model, ecfg.dim
        );
    }

    // 启动后台 CDC 同步循环（若已配置）。
    if let Some((rcfg, interval)) = cdc {
        eprintln!(
            "cdc on: slot={} publication={} interval={:?}",
            rcfg.slot, rcfg.publication, interval
        );
        state.spawn_cdc(rcfg, data.clone(), interval);
    }

    let app = router(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!(
        "fastsearch-server listening on http://{addr}  (data: {})",
        data.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
