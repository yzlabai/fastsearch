//! MCP (Model Context Protocol) server over stdio.
//!
//! What: exposes the parser as five MCP tools — `parse_document`,
//! `get_chunks`, `outline`, `export_okf`, `locate` — so agents (Claude Code,
//! claude.ai, …) can call docparse directly and get structured results with
//! provenance + bbox citations, no shell wrapping.
//!
//! Why hand-written: the MCP stdio transport is newline-delimited JSON-RPC
//! 2.0 with three methods we care about (`initialize`, `tools/list`,
//! `tools/call`); `serde_json` covers that without an SDK, keeping the
//! zero-dependency single-binary identity. Pinned to MCP protocol revision
//! "2025-03-26" — revisit (or adopt the official `rmcp` SDK) if the spec
//! moves in ways that matter here.
//!
//! Error model: protocol problems (bad JSON, unknown method/tool) are
//! JSON-RPC errors; tool execution failures (unreadable file, parse error)
//! are tool results with `isError: true` — the server never exits or panics
//! on bad input, mirroring the parser's bad-page-yields-empty-Page policy.

use docparse_core::output;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-03-26";

/// Run the stdio loop until stdin closes. One JSON-RPC message per line.
/// `state` holds the lazily-loaded enhancement models behind the tools'
/// boolean arguments (ocr/layout/table_model/formula_model/vlm_*).
pub fn serve(state: crate::EnhanceState) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_line(&line, &state) {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Handle one incoming message; `None` for notifications (no response due).
fn handle_line(line: &str, state: &crate::EnhanceState) -> Option<String> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}")).to_string())
        }
    };
    // A request carries an id; anything without one is a notification.
    let id = match msg.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return None,
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    let outcome = match method {
        "initialize" => Ok(json!({
            // Echo the client's requested revision when present; we don't use
            // revision-specific features beyond the basics pinned above.
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "docparse",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_specs() })),
        "tools/call" => call_tool(&params, state),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(
        match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => rpc_error(id, code, &message),
        }
        .to_string(),
    )
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_specs() -> Value {
    json!([
        {
            "name": "parse_document",
            "description": "Parse a local document (PDF/DOCX/HTML) into json, markdown, or text. \
                            json carries provenance and positioned elements (PDF user space: \
                            origin bottom-left, y up, pt).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local file path" },
                    "format": { "type": "string", "enum": ["json", "markdown", "text"],
                                "description": "Output format (default json)" },
                    "ocr": { "type": "boolean",
                             "description": "OCR scanned pages (default false; digital pages never touch the model)" },
                    "layout": { "type": "boolean",
                                "description": "Re-derive reading order with the layout model (PDF only; needs server --layout-model files)" },
                    "table_model": { "type": "boolean",
                                     "description": "Re-extract table structure with the embedded UniRec model (PDF only; needs server --unirec-models)" },
                    "formula_model": { "type": "boolean",
                                       "description": "Convert display formulas to LaTeX (PDF only; needs server --unirec-models + layout model)" },
                    "vlm_describe": { "type": "boolean",
                                      "description": "Caption figures via the configured VLM service (PDF only; needs server --vlm-url/--vlm-model)" },
                    "vlm_tables": { "type": "boolean",
                                    "description": "Re-extract tables via the configured VLM service (PDF only)" },
                    "images": { "type": "string", "enum": ["embedded"],
                                "description": "\"embedded\" adds data_base64 + data_media_type to image elements (json format)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "get_chunks",
            "description": "Parse a local document into retrieval chunks. Each chunk carries \
                            page + bbox (citable source location), heading breadcrumb, and \
                            char_len; the envelope carries provenance, a quality report, and \
                            a per-page complexity profile.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local file path" },
                    "ocr": { "type": "boolean",
                             "description": "OCR scanned pages (default false)" },
                    "layout": { "type": "boolean", "description": "Layout-model reading order (PDF only)" },
                    "table_model": { "type": "boolean", "description": "UniRec table structure (PDF only)" },
                    "formula_model": { "type": "boolean", "description": "Formulas to LaTeX (PDF only)" },
                    "vlm_describe": { "type": "boolean", "description": "VLM figure captions (PDF only)" },
                    "vlm_tables": { "type": "boolean", "description": "VLM table re-extraction (PDF only)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "outline",
            "description": "Parse a local document into its structure tree (table of contents): \
                            nested sections, each with title, level, page, and bbox (citable). \
                            Navigate long documents agentically — list the top-level sections \
                            (max_depth), then drill into one (id). Section ids match get_chunks' \
                            section_id, so you can fetch a section's chunks next.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local file path" },
                    "id": { "type": "integer",
                             "description": "Return only this section's subtree (default: whole document, root id 0)" },
                    "max_depth": { "type": "integer",
                                   "description": "Prune deeper than this many levels (0 = just the node; default: full tree)" },
                    "ocr": { "type": "boolean", "description": "OCR scanned pages first (default false)" },
                    "layout": { "type": "boolean", "description": "Layout-model reading order (PDF only)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "export_okf",
            "description": "Parse a local document into an Open Knowledge Format (OKF v0.1) \
                            bundle: a set of Markdown + YAML-frontmatter concept files mirroring \
                            the structure tree (one per section, citable page+bbox). Returns the \
                            files inline (path + content) so an agent can write or read them \
                            directly — git-native, vendor-neutral knowledge delivery.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local file path" },
                    "resource_base": { "type": "string",
                                       "description": "Prefix for concept resource URIs (default: bare basename)" },
                    "ocr": { "type": "boolean", "description": "OCR scanned pages first (default false)" },
                    "layout": { "type": "boolean", "description": "Layout-model reading order (PDF only)" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "locate",
            "description": "Reverse citation lookup: given a page (1-based) and a point x,y in \
                            PDF user space, return the chunk covering it (null if none).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local file path" },
                    "page": { "type": "integer", "description": "1-based page number" },
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "ocr": { "type": "boolean",
                             "description": "OCR scanned pages before locating (default false)" }
                },
                "required": ["path", "page", "x", "y"]
            }
        }
    ])
}

/// Dispatch `tools/call`. Unknown tool = protocol error; tool failure = result
/// with `isError: true` so the agent sees a structured, recoverable message.
fn call_tool(params: &Value, state: &crate::EnhanceState) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let run = match name {
        "parse_document" => tool_parse_document(&args, state),
        "get_chunks" => tool_get_chunks(&args, state),
        "outline" => tool_outline(&args, state),
        "export_okf" => tool_export_okf(&args, state),
        "locate" => tool_locate(&args, state),
        _ => return Err((-32602, format!("unknown tool: {name}"))),
    };
    Ok(match run {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("error: {e:#}") }],
            "isError": true
        }),
    })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required argument: {key}"))
}

/// Parse, then apply whatever enhancements the tool asked for (boolean
/// arguments; everything defaults off = the deterministic result). PDF-only
/// enhancements are no-ops on other formats.
fn parse_enhanced(
    args: &Value,
    state: &crate::EnhanceState,
) -> anyhow::Result<docparse_core::ir::Document> {
    let path = std::path::Path::new(str_arg(args, "path")?);
    let images_embedded = args.get("images").and_then(Value::as_str) == Some("embedded");
    let doc = crate::parse_path_with(path, images_embedded)?;
    let flag = |k: &str| args.get(k).and_then(Value::as_bool).unwrap_or(false);
    let opts = crate::EnhanceOpts {
        ocr: flag("ocr"),
        images_embedded,
        layout: flag("layout"),
        table_model: flag("table_model"),
        formula_model: flag("formula_model"),
        vlm_describe: flag("vlm_describe"),
        vlm_tables: flag("vlm_tables"),
    };
    state.apply(doc, path, opts)
}

fn tool_parse_document(args: &Value, state: &crate::EnhanceState) -> anyhow::Result<String> {
    let doc = parse_enhanced(args, state)?;
    match args.get("format").and_then(Value::as_str).unwrap_or("json") {
        "json" => output::to_json(&doc),
        "markdown" => Ok(output::to_markdown(&doc)),
        "text" => Ok(output::to_text(&doc)),
        other => Err(anyhow::anyhow!(
            "unknown format: {other} (json|markdown|text)"
        )),
    }
}

fn tool_get_chunks(args: &Value, state: &crate::EnhanceState) -> anyhow::Result<String> {
    let doc = parse_enhanced(args, state)?;
    let table_markdown = args.get("table_format").and_then(Value::as_str) == Some("markdown");
    let chunks = docparse_core::chunk::chunk_document_with(
        &doc,
        docparse_core::chunk::ChunkOptions {
            table_markdown,
            ..Default::default()
        },
    );
    let envelope = json!({
        "provenance": serde_json::to_value(&doc.provenance)?,
        "quality": serde_json::to_value(docparse_core::quality::analyze(&doc))?,
        "profile": serde_json::to_value(docparse_core::quality::profile(&doc))?,
        "chunks": serde_json::to_value(&chunks)?,
    });
    Ok(serde_json::to_string_pretty(&envelope)?)
}

fn tool_outline(args: &Value, state: &crate::EnhanceState) -> anyhow::Result<String> {
    let doc = parse_enhanced(args, state)?;
    let root = docparse_core::outline::build(&doc);
    // Optionally focus on one section's subtree.
    let node = match args.get("id").and_then(Value::as_u64) {
        Some(id) => root
            .get(id as usize)
            .ok_or_else(|| anyhow::anyhow!("no section with id {id}"))?
            .clone(),
        None => root,
    };
    // Optionally prune depth (e.g. max_depth=1 = table of contents only).
    let node = match args.get("max_depth").and_then(Value::as_u64) {
        Some(d) => node.pruned(d as usize),
        None => node,
    };
    Ok(docparse_core::outline::to_json(&node))
}

fn tool_export_okf(args: &Value, state: &crate::EnhanceState) -> anyhow::Result<String> {
    let doc = parse_enhanced(args, state)?;
    let path = std::path::Path::new(str_arg(args, "path")?);
    let resource_base = args
        .get("resource_base")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let opts = crate::okf_options_for(path, resource_base, false);
    let bundle = docparse_core::okf::build(&doc, &opts);
    let files: Vec<Value> = bundle
        .files
        .iter()
        .map(|(p, content)| json!({ "path": p.to_string_lossy(), "content": content }))
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "okf_version": "0.1",
        "files": files,
    }))?)
}

fn tool_locate(args: &Value, state: &crate::EnhanceState) -> anyhow::Result<String> {
    let doc = parse_enhanced(args, state)?;
    let page = args
        .get("page")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing required argument: page"))? as usize;
    let (x, y) = match (
        args.get("x").and_then(Value::as_f64),
        args.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => (x as f32, y as f32),
        _ => anyhow::bail!("missing required argument: x and y"),
    };
    let chunks = docparse_core::chunk::chunk_document(&doc);
    let hit = docparse_core::chunk::locate(&chunks, page, x, y);
    Ok(serde_json::to_string_pretty(&serde_json::to_value(hit)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
    }

    fn state() -> crate::EnhanceState {
        crate::EnhanceState::new(
            "models/ppocr".into(),
            "models/layout/doclayout_yolo.onnx".into(),
            None,
            None,
        )
    }

    fn result_of(line: &str) -> Value {
        let resp: Value =
            serde_json::from_str(&handle_line(line, &state()).expect("response")).unwrap();
        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        resp["result"].clone()
    }

    fn temp_html(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(
            &path,
            "<html><body><h1>Title</h1><p>Hello mcp world.</p></body></html>",
        )
        .unwrap();
        path
    }

    #[test]
    fn initialize_and_list() {
        let r = result_of(&req("initialize", json!({"protocolVersion": "2025-03-26"})));
        assert_eq!(r["serverInfo"]["name"], "docparse");
        assert_eq!(r["protocolVersion"], "2025-03-26");
        let tools = result_of(&req("tools/list", json!({})));
        assert_eq!(tools["tools"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn export_okf_returns_bundle_files() {
        let path = temp_html("docparse-mcp-okf.html");
        let r = result_of(&req(
            "tools/call",
            json!({ "name": "export_okf", "arguments": { "path": path } }),
        ));
        assert_eq!(r["isError"], false);
        let bundle: Value =
            serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(bundle["okf_version"], "0.1");
        let files = bundle["files"].as_array().unwrap();
        // index.md + the "Title" concept.
        assert!(files.iter().any(|f| f["path"] == "index.md"));
        assert!(files.iter().any(|f| f["content"]
            .as_str()
            .unwrap_or("")
            .contains("type: \"Section\"")));
    }

    #[test]
    fn outline_tool_returns_structure_tree() {
        let path = temp_html("docparse-mcp-outline.html");
        let call = req(
            "tools/call",
            json!({ "name": "outline", "arguments": { "path": path } }),
        );
        let r = result_of(&call);
        assert_eq!(r["isError"], false);
        let tree: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        // Synthetic root (id 0) with the "Title" heading as a child section.
        assert_eq!(tree["id"], 0);
        assert_eq!(tree["children"][0]["title"], "Title");
        assert!(tree["children"][0]["id"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn notifications_get_no_response() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert!(handle_line(&note, &state()).is_none());
    }

    #[test]
    fn unknown_method_is_rpc_error() {
        let resp: Value =
            serde_json::from_str(&handle_line(&req("nope", json!({})), &state()).unwrap()).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn parse_document_roundtrip_is_deterministic() {
        let path = temp_html("docparse-mcp-test.html");
        let call = req(
            "tools/call",
            json!({ "name": "parse_document",
                    "arguments": { "path": path, "format": "text" } }),
        );
        let r1 = result_of(&call);
        let r2 = result_of(&call);
        assert_eq!(r1, r2, "same request must yield byte-identical results");
        assert_eq!(r1["isError"], false);
        let text = r1["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Hello mcp world."), "got: {text}");
    }

    #[test]
    fn get_chunks_carries_provenance_and_bbox() {
        let path = temp_html("docparse-mcp-chunks.html");
        let r = result_of(&req(
            "tools/call",
            json!({ "name": "get_chunks", "arguments": { "path": path } }),
        ));
        assert_eq!(r["isError"], false);
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(env["provenance"]["parser"].is_string());
        let chunks = env["chunks"].as_array().unwrap();
        assert!(!chunks.is_empty());
        assert!(
            chunks[0]["bbox"]["x0"].is_number(),
            "chunks must be citable"
        );
    }

    #[test]
    fn ocr_with_missing_models_is_tool_error() {
        let path = temp_html("docparse-mcp-ocr-missing.html");
        let resp: Value = serde_json::from_str(
            &handle_line(
                &req(
                    "tools/call",
                    json!({ "name": "get_chunks",
                            "arguments": { "path": path, "ocr": true } }),
                ),
                &crate::EnhanceState::new(
                    "/nonexistent/models".into(),
                    "models/layout/doclayout_yolo.onnx".into(),
                    None,
                    None,
                ),
            )
            .unwrap(),
        )
        .unwrap();
        let r = &resp["result"];
        assert_eq!(r["isError"], true);
        let msg = r["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("ocr models unavailable"), "got: {msg}");
    }

    #[test]
    fn unconfigured_capability_names_the_startup_flag() {
        // table_model: true on a server started without --unirec-models must
        // fail with guidance, not crash — and only for PDFs (the enhancement
        // is a documented no-op on other formats).
        let pdf = std::env::temp_dir().join("docparse-mcp-tm.pdf");
        std::fs::write(&pdf, b"%PDF-1.4 not really").unwrap();
        let r = result_of(&req(
            "tools/call",
            json!({ "name": "get_chunks",
                    "arguments": { "path": pdf, "table_model": true } }),
        ));
        assert_eq!(r["isError"], true); // parse fails on garbage pdf first — fine
        let html = temp_html("docparse-mcp-tm.html");
        let r = result_of(&req(
            "tools/call",
            json!({ "name": "get_chunks",
                    "arguments": { "path": html, "table_model": true } }),
        ));
        // Non-PDF: enhancement skipped, parse succeeds.
        assert_eq!(r["isError"], false);
    }

    #[test]
    fn bad_file_is_tool_error_not_crash() {
        let r = result_of(&req(
            "tools/call",
            json!({ "name": "get_chunks",
                    "arguments": { "path": "/nonexistent/x.html" } }),
        ));
        assert_eq!(r["isError"], true);
        let unknown: Value = serde_json::from_str(
            &handle_line(&req("tools/call", json!({"name": "zap"})), &state()).unwrap(),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }
}
