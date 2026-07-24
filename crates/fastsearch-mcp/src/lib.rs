//! # fastsearch-mcp
//!
//! 第四张脸：**MCP（Model Context Protocol）服务**，stdio + JSON-RPC 2.0，把混合检索暴露为
//! LLM 可直接调用的工具（`search` / `resolve_citation`）。薄适配引擎，逻辑在 lib（纯、可单测），
//! main 只是 stdio 收发壳。
//!
//! **ACL 不可绕过（守不变量 #3）**：principal/ACL 由**服务端配置**注入 `engine.search`/
//! `resolve_citation`，MCP 客户端（LLM）的工具入参里**不接受也无法放宽** ACL——与 REST 一致。
//!
//! 协议：`initialize` 握手 → `tools/list` 列工具 → `tools/call` 调用；`ping` 存活；
//! 通知（`notifications/*`）无响应。详见架构大图"四张脸"。

use fastsearch_core::{AclFilter, SearchRequest};
use fastsearch_engine::{AssetFetch, Engine};
use serde_json::{json, Value};

/// 支持的 MCP 协议版本（与主流客户端对齐）。
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// MCP 服务：持有引擎 + **服务端固定 ACL**（None=本地全量访问，由部署方决定）。
pub struct McpServer {
    engine: Engine,
    acl: Option<AclFilter>,
}

impl McpServer {
    pub fn new(engine: Engine, acl: Option<AclFilter>) -> Self {
        McpServer { engine, acl }
    }

    /// 处理一条 JSON-RPC 消息：**请求**返回 `Some(响应)`，**通知**（无 `id`）返回 `None`。
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // 通知（如 notifications/initialized）：无 id、无响应。
        if id.is_none() && method.starts_with("notifications/") {
            return None;
        }
        let result = match method {
            "initialize" => Ok(self.initialize_result()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => return Some(self.tools_call(id, msg.get("params"))),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(v) => ok(id, v),
            Err((code, m)) => err(id, code, &m),
        })
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "fastsearch-mcp", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    /// `tools/call`：分派到具体工具。工具内部错误以 `isError:true` 的内容返回（MCP 约定：
    /// 工具执行失败不发协议级 error，便于 LLM 读到失败原因）。
    fn tools_call(&self, id: Option<Value>, params: Option<&Value>) -> Value {
        let Some(params) = params else {
            return err(id, -32602, "missing params");
        };
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = match name {
            "search" => self.tool_search(args),
            "resolve_citation" => self.tool_resolve_citation(args),
            other => Err(format!("unknown tool: {other}")),
        };
        match outcome {
            Ok(text) => ok(id, tool_text(&text, false)),
            // 工具级失败：result + isError，不是协议 error。
            Err(e) => ok(id, tool_text(&e, true)),
        }
    }

    /// `search` 工具：入参即 `SearchRequest`（query 必填，mode/top_k/filter 可选）；ACL 服务端注入。
    fn tool_search(&self, args: Value) -> Result<String, String> {
        let req: SearchRequest =
            serde_json::from_value(args).map_err(|e| format!("invalid search args: {e}"))?;
        let hits = self
            .engine
            .search(&req, self.acl.as_ref())
            .map_err(|e| format!("search failed: {e}"))?;
        let arr: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "citation_id": h.citation.citation_id(),
                    "score": h.score,
                    "page": h.citation.page,
                    "heading_path": h.citation.heading_path,
                    "snippet": h.highlight,
                })
            })
            .collect();
        serde_json::to_string(&json!({ "hits": arr })).map_err(|e| e.to_string())
    }

    /// `resolve_citation` 工具：由 citation_id 解析媒资/原文位置（ACL 服务端强制，越权/不存在
    /// 均报"未找到或无权限"，不暴露存在性）。
    fn tool_resolve_citation(&self, args: Value) -> Result<String, String> {
        let cid = args
            .get("citation_id")
            .and_then(|c| c.as_str())
            .ok_or("missing citation_id")?;
        let resolved = self
            .engine
            .resolve_citation(cid, self.acl.as_ref())
            .map_err(|e| format!("resolve failed: {e}"))?;
        let v = match resolved {
            None => json!({ "found": false, "reason": "not found or not authorized" }),
            Some(a) => {
                let fetch = match a.fetch {
                    AssetFetch::DocRender { doc_id, page, bbox } => json!({
                        "kind": "doc_render", "doc_id": doc_id, "page": page, "bbox": bbox,
                    }),
                    AssetFetch::SignedUrl { url, expires_s } => json!({
                        "kind": "signed_url", "url": url, "expires_s": expires_s,
                    }),
                    AssetFetch::InlineRef => json!({
                        // inline 小图：字节在 PG 真源，经 REST `GET /v1/asset/{cid}` 取（MCP 只给指针）。
                        "kind": "inline_ref",
                    }),
                };
                json!({ "found": true, "media_type": a.media_type, "time": a.time, "fetch": fetch })
            }
        };
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }
}

/// 两个工具的定义（名称/描述/入参 JSON Schema）。
pub fn tool_defs() -> Value {
    json!([
        {
            "name": "search",
            "description": "在 fastsearch 混合检索引擎中检索（keyword/vector/hybrid），返回带引用\
                （citation_id/page/heading_path/snippet）的命中，供答案层溯源。ACL 由服务端强制。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "检索词/问题" },
                    "mode": { "type": "string", "enum": ["keyword", "vector", "hybrid"], "default": "hybrid" },
                    "top_k": { "type": "integer", "default": 20 },
                    "filter": { "type": "object", "description": "core::Filter AST（可选）" },
                    "highlight": { "type": "boolean", "default": false }
                },
                "required": ["query"]
            }
        },
        {
            "name": "resolve_citation",
            "description": "由 citation_id 解析媒资/原文位置（page+bbox 或签名 URL），用于深链/打开\
                原文。ACL 由服务端强制，越权/不存在均返回 found:false。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "citation_id": { "type": "string", "description": "collection:doc_id:chunk_id" }
                },
                "required": ["citation_id"]
            }
        }
    ])
}

/// MCP `tools/call` 结果：单个文本内容块 + isError 标志。
fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

/// JSON-RPC 成功响应。
fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

/// JSON-RPC 错误响应。
fn err(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastsearch_core::{BBox, Chunk, ChunkKind};
    use fastsearch_text::{TextIndexConfig, TokenizerKind};

    fn chunk(doc: &str, id: u64, text: &str) -> Chunk {
        Chunk {
            doc_id: doc.into(),
            chunk_id: id,
            kind: ChunkKind::Paragraph,
            text: text.into(),
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            heading_path: vec!["财务".into()],
            section_id: 0,
            char_len: text.chars().count() as u32,
            media: None,
            media_bytes: None,
            image_vector_status: None,
            tenant: None,
            acl: vec!["public".into()],
            metadata: Default::default(),
            searchable: true,
        }
    }

    fn server() -> McpServer {
        let cfg = TextIndexConfig {
            tokenizer: TokenizerKind::Jieba,
            ..Default::default()
        };
        let mut e = Engine::create_in_ram(cfg).unwrap();
        e.ingest("kb", &chunk("r.pdf", 1, "毛利率提升至 42%"))
            .unwrap();
        e.ingest("kb", &chunk("r.pdf", 2, "营业收入增长")).unwrap();
        e.commit().unwrap();
        McpServer::new(e, None)
    }

    #[test]
    fn initialize_and_tools_list() {
        let s = server();
        let init = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(init["result"]["serverInfo"]["name"], "fastsearch-mcp");

        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"search") && names.contains(&"resolve_citation"));
    }

    #[test]
    fn notification_has_no_response() {
        let s = server();
        assert!(s
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
    }

    #[test]
    fn unknown_method_errors() {
        let s = server();
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":9,"method":"bogus"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_search_returns_hits() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params": { "name": "search", "arguments": { "query": "毛利率", "mode": "keyword" } }
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let hits = parsed["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0]["citation_id"], "kb:r.pdf:1");
    }

    #[test]
    fn tools_call_bad_args_is_tool_error_not_protocol_error() {
        let s = server();
        // 缺 query → SearchRequest 反序列化失败 → isError:true（工具级），仍是 result 不是 error。
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params": { "name": "search", "arguments": { "mode": "keyword" } }
            }))
            .unwrap();
        assert!(r.get("error").is_none());
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn tools_call_resolve_citation_no_media() {
        let s = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":5,"method":"tools/call",
                "params": { "name": "resolve_citation", "arguments": { "citation_id": "kb:r.pdf:1" } }
            }))
            .unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        // chunk 无 media → found:false（无媒资），但非协议错误。
        assert_eq!(parsed["found"], false);
    }
}
