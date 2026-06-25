//! fastsearch-server 二进制：起 REST 服务（四张脸之一）。
//!
//! 配置（环境变量）：
//! - `FASTSEARCH_DATA`：索引数据目录（默认 `./data`）。
//! - `FASTSEARCH_PORT`：监听端口（默认 8642）。
//! - `FASTSEARCH_KEYS`：API Key 表，格式 `key=tenant:tag1,tag2;key2=:public`
//!   （tenant 留空=管理员/无租户限制）。未设则建一个 dev key `dev`（无租户限制）。
//! - `FASTSEARCH_RATE_LIMIT`：`capacity,refill_per_sec`（每 key 令牌桶）；未设=不限流。
//! - `FASTSEARCH_AUDIT`：设为 `1`/`stderr` 则每个成功请求向 stderr 输出一行审计 JSON。
//! - `FASTSEARCH_EMBEDDER` = `hash`|`ollama`|`openai`（+ `FASTSEARCH_EMBED_*`）：真语义嵌入后端。
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
    let cfg = TextIndexConfig {
        tokenizer,
        ..Default::default()
    };
    // 打开数据目录下的派生索引（落盘恢复）：text + vector.bin + checkpoint.json。
    let (mut engine, start_lsn) = Engine::open(&data, cfg)?;

    // 嵌入后端配置（FASTSEARCH_EMBEDDER=ollama|openai；默认 hash→不嵌入）。
    let ecfg = fastsearch_embed::EmbedderConfig::from_env();
    let embed_on = matches!(ecfg.kind, fastsearch_embed::EmbedderKind::Http(_));
    if embed_on {
        // CDC 落地路径用引擎自身的 embedder 嵌入 passage。
        engine.set_embedder(fastsearch_embed::build_embedder(&ecfg));
    }

    let mut state = ServerState::new(engine, keys);

    // 限流：FASTSEARCH_RATE_LIMIT="capacity,refill_per_sec"
    if let Ok(spec) = std::env::var("FASTSEARCH_RATE_LIMIT") {
        if let Some((cap, refill)) = spec.split_once(',') {
            if let (Ok(cap), Ok(refill)) = (cap.trim().parse(), refill.trim().parse()) {
                state = state.with_rate_limit(cap, refill);
                eprintln!("rate limit on: capacity={cap}, refill={refill}/s per key");
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

    // 后台 CDC 同步循环：FASTSEARCH_CDC=1 + DATABASE_URL（+ 可选 SLOT/PUBLICATION/INTERVAL_MS）。
    if matches!(std::env::var("FASTSEARCH_CDC").as_deref(), Ok("1")) {
        match std::env::var("DATABASE_URL") {
            Ok(url) => {
                let rcfg = fastsearch_sync::replication::ReplicationConfig {
                    url,
                    slot: std::env::var("FASTSEARCH_CDC_SLOT")
                        .unwrap_or_else(|_| "fastsearch_slot".into()),
                    publication: std::env::var("FASTSEARCH_CDC_PUBLICATION")
                        .unwrap_or_else(|_| "fastsearch_pub".into()),
                };
                let interval_ms: u64 = std::env::var("FASTSEARCH_CDC_INTERVAL_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1000);
                fastsearch_sync::replication::ensure_slot(&rcfg).await?;
                eprintln!(
                    "cdc on: slot={} publication={} interval={interval_ms}ms (resume lsn={start_lsn:?})",
                    rcfg.slot, rcfg.publication
                );
                state.spawn_cdc(
                    rcfg,
                    data.clone(),
                    start_lsn,
                    std::time::Duration::from_millis(interval_ms),
                );
            }
            Err(_) => eprintln!("FASTSEARCH_CDC=1 但未设 DATABASE_URL，跳过 CDC"),
        }
    }

    let app = router(state);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!(
        "fastsearch-server listening on http://{addr}  (data: {})",
        data.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
