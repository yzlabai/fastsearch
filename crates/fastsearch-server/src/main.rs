//! fastsearch-server 二进制：起 REST 服务（四张脸之一）。
//!
//! 配置（环境变量）：
//! - `FASTSEARCH_DATA`：索引数据目录（默认 `./data`）。
//! - `FASTSEARCH_PORT`：监听端口（默认 8642）。
//! - `FASTSEARCH_KEYS`：API Key 表，格式 `key=tenant:tag1,tag2;key2=:public`
//!   （tenant 留空=管理员/无租户限制）。未设则建一个 dev key `dev`（无租户限制）。

use fastsearch_engine::Engine;
use fastsearch_server::{router, Principal, ServerState};
use fastsearch_text::{TextIndexConfig, TokenizerKind};
use std::collections::HashMap;
use std::path::PathBuf;

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
    std::fs::create_dir_all(data.join("text"))?;
    let engine = Engine::open_or_create(&data.join("text"), cfg)?;
    let app = router(ServerState::new(engine, keys));

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!(
        "fastsearch-server listening on http://{addr}  (data: {})",
        data.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}
