//! fastsearch MCP 服务（stdio + JSON-RPC 2.0）。逻辑在 [`fastsearch_mcp`]，main 只做收发。
//!
//! 环境变量：
//! - `FASTSEARCH_DATA`：索引数据目录（默认 `./data`）。
//! - `FASTSEARCH_TOKENIZER` = `jieba`(默认)|`default`。
//! - `FASTSEARCH_MCP_TENANT` / `FASTSEARCH_MCP_TAGS`(逗号分隔)：**服务端固定 ACL**（不设=本地
//!   全量访问）。客户端无法在工具入参里传/放宽 ACL（守不变量 #3）。
//!
//! 传输：stdio，**一行一个 JSON-RPC 消息**（line-delimited）。配进 MCP 客户端的 stdio server。

use std::io::{BufRead, Write};
use std::path::PathBuf;

use fastsearch_core::AclFilter;
use fastsearch_engine::Engine;
use fastsearch_mcp::McpServer;
use fastsearch_text::{TextIndexConfig, TokenizerKind};

fn main() -> anyhow::Result<()> {
    let data: PathBuf = std::env::var("FASTSEARCH_DATA")
        .unwrap_or_else(|_| "./data".into())
        .into();
    let tokenizer = match std::env::var("FASTSEARCH_TOKENIZER").as_deref() {
        Ok("default") => TokenizerKind::Default,
        _ => TokenizerKind::Jieba,
    };
    let cfg = TextIndexConfig {
        tokenizer,
        ..Default::default()
    };
    let (engine, _lsn) = Engine::open(&data, cfg)?;

    // 服务端固定 ACL（不设 → None=本地全量）。客户端不可绕过。
    let acl = match std::env::var("FASTSEARCH_MCP_TENANT") {
        Ok(t) if !t.is_empty() => Some(AclFilter {
            tenant: Some(t),
            allowed_tags: std::env::var("FASTSEARCH_MCP_TAGS")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default(),
        }),
        _ => None,
    };

    let server = McpServer::new(engine, acl);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    eprintln!("fastsearch-mcp ready (stdio); data={}", data.display());

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // 解析失败：回 JSON-RPC parse error（id 未知→null）。
                let resp = serde_json::json!({
                    "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(resp) = server.handle(&msg) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
