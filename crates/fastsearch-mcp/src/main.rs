//! fastsearch MCP 服务（stdio + JSON-RPC 2.0）。逻辑在 [`fastsearch_mcp`]，main 只做收发。
//!
//! **两个运行档**（KB-0.2，见 [22-mcp spec](../../../docs/specs/22-mcp.md)）：
//! - **远端档（推荐）**：设 `FASTSEARCH_SERVER` → 作为 server 的纯 REST 客户端。
//!   身份=API key，ACL 由 server 从认证身份注入；hybrid 由 server 的嵌入后端免费提供。
//! - **本地档**：不设 `FASTSEARCH_SERVER` → 进程内嵌引擎读本地索引目录，单机离线可用；
//!   只能 keyword（引擎从不嵌入文本 query），ACL 是进程级常量。
//!
//! 环境变量：
//! - `FASTSEARCH_SERVER` / `FASTSEARCH_KEY`：远端档的 server 地址与 API key（同 CLI）。
//! - `FASTSEARCH_TIMEOUT_SECS`：远端档 HTTP 超时（默认 30s）。
//! - `FASTSEARCH_DATA`：本地档索引数据目录（默认 `./data`）。
//! - `FASTSEARCH_TOKENIZER` = `jieba`(默认)|`default`（本地档）。
//! - `FASTSEARCH_MCP_TENANT` / `FASTSEARCH_MCP_TAGS`(逗号分隔)：**本地档**的服务端固定 ACL。
//!   客户端无法在工具入参里传/放宽 ACL（守不变量 #3）。
//! - `FASTSEARCH_MCP_ACL=all`：本地档显式声明"单机全量访问"。**未设 tenant 也未设它 → 拒绝启动**
//!   （fail-closed：既然 100% 依赖调用方接对，就绝不能在他没接对时替他编一个身份）。
//!
//! 传输：stdio，**一行一个 JSON-RPC 消息**（line-delimited）。配进 MCP 客户端的 stdio server。

use std::io::{BufRead, Write};
use std::path::PathBuf;

use fastsearch_core::AclFilter;
use fastsearch_engine::Engine;
use fastsearch_mcp::McpServer;
use fastsearch_text::{TextIndexConfig, TokenizerKind};

/// 本地档的 ACL：显式 tenant，或显式 `FASTSEARCH_MCP_ACL=all`，**否则拒绝启动**。
///
/// 此前未设 tenant → `acl = None` → `engine.search(&req, None)` **不做任何 ACL 判定、全库可见**。
/// 这正是 server 侧 fail-closed 修掉的那类"替调用方猜"（`FASTSEARCH_KEYS` 未设 → 拒绝启动），
/// 而 ADR《职责边界》"不豁免"第 1 条写明：既然 100% 依赖调用方接对，就绝不能在他没接对时替他编一个。
fn local_acl() -> anyhow::Result<Option<AclFilter>> {
    if let Ok(t) = std::env::var("FASTSEARCH_MCP_TENANT") {
        if !t.trim().is_empty() {
            return Ok(Some(AclFilter {
                tenant: Some(t),
                allowed_tags: std::env::var("FASTSEARCH_MCP_TAGS")
                    .map(|s| {
                        s.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
            }));
        }
    }
    if std::env::var("FASTSEARCH_MCP_ACL").as_deref() == Ok("all") {
        return Ok(None); // 显式声明的单机全量：写出来才算数。
    }
    anyhow::bail!(
        "本地档必须显式声明可见范围，否则拒绝启动（fail-closed）。三选一：\n\
         \x20 ① 多租户：FASTSEARCH_MCP_TENANT=acme FASTSEARCH_MCP_TAGS=team-a,public\n\
         \x20 ② 单机全量：FASTSEARCH_MCP_ACL=all（显式声明「本机全部数据可见」）\n\
         \x20 ③ 改走远端档：FASTSEARCH_SERVER=http://localhost:8642 FASTSEARCH_KEY=<key>\n\
         此前未设 tenant 时会静默变成「全库可见」——那是替调用方猜身份，已按 ADR《职责边界》断根。"
    )
}

fn main() -> anyhow::Result<()> {
    // 档位选择：设了 FASTSEARCH_SERVER 即远端档。
    let server_env = std::env::var("FASTSEARCH_SERVER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if let Some(server) = server_env {
        // 远端档下 FASTSEARCH_MCP_TENANT/_TAGS 是**客户端自称的身份**——无意义且危险。
        // 不合并、不静默取其一：让部署方自己二选一。
        if std::env::var("FASTSEARCH_MCP_TENANT").is_ok_and(|v| !v.trim().is_empty())
            || std::env::var("FASTSEARCH_MCP_TAGS").is_ok_and(|v| !v.trim().is_empty())
        {
            anyhow::bail!(
                "同时配置了远端档（FASTSEARCH_SERVER）与本地档 ACL（FASTSEARCH_MCP_TENANT/_TAGS），\n\
                 拒绝启动。远端档的身份是那把 API key，ACL 由 server 从认证身份注入；\n\
                 客户端自称的 tenant/tags 在那里既无意义也危险。二选一：\n\
                 \x20 ① 走远端：删掉 FASTSEARCH_MCP_TENANT/_TAGS，用 FASTSEARCH_KEY\n\
                 \x20 ② 走本地：删掉 FASTSEARCH_SERVER"
            );
        }
        let backend = fastsearch_mcp::RemoteBackend::connect(Some(server.clone()), None)?;
        let caps = backend.caps().clone();
        let server_obj =
            McpServer::with_backend(fastsearch_mcp::Backend::Remote(Box::new(backend)));
        eprintln!(
            "fastsearch-mcp ready (stdio); 远端档 server={server} embedded={} backend={} 真源={}",
            caps.embedded, caps.vector_backend, caps.source_of_truth
        );
        return serve_stdio(&server_obj);
    }

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

    // 本地档固定 ACL（fail-closed，见 `local_acl`）。客户端不可绕过。
    let acl = local_acl()?;
    let scope = match &acl {
        Some(a) => format!("tenant={:?}", a.tenant.as_deref().unwrap_or("-")),
        None => "全量（FASTSEARCH_MCP_ACL=all）".to_string(),
    };
    let server = McpServer::new(engine, acl);
    eprintln!(
        "fastsearch-mcp ready (stdio); 本地档 data={} 可见范围={scope}；仅 keyword 档，\
         语义/混合检索请改走远端档（FASTSEARCH_SERVER）",
        data.display()
    );
    serve_stdio(&server)
}

/// stdio 收发循环：一行一个 JSON-RPC 消息。
fn serve_stdio(server: &McpServer) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
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
