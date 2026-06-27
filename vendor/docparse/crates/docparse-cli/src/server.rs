//! REST server (roadmap module 10, plan N2b): an HTTP face over the same
//! pipeline as the CLI and the MCP server.
//!
//! Surface: `POST /parse?format=json|markdown|text|chunks` (multipart file
//! upload) and `GET /healthz`. For the same input and format the response
//! body is byte-identical to the CLI's stdout — determinism holds across
//! interfaces, and tests pin it.
//!
//! Scope (plan §3/§5): no auth/multi-tenancy/queueing ("don't build the
//! orchestration machine early"). Binds 127.0.0.1 by default — same-machine
//! trust model, like the CLI; `serve --host 0.0.0.0` opts into a wider bind
//! for a trusted network boundary (e.g. a container on a private compose
//! network). Parsing runs in `spawn_blocking` (rayon inside is CPU-bound).
//! Security pre-checks (zip bombs etc.) are N5.

use axum::extract::{DefaultBodyLimit, Multipart, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use docparse_core::output;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub fn serve(host: &str, port: u16, state: crate::EnhanceState) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let listener = tokio::net::TcpListener::bind((host, port)).await?;
            eprintln!("docparse REST listening on http://{host}:{port}");
            axum::serve(listener, router(state)).await?;
            Ok(())
        })
}

fn router(state: crate::EnhanceState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/parse", post(parse))
        // Self-describing contract for external agents: the OpenAPI document and
        // the individual output JSON Schemas (same source as `docparse schema`).
        .route("/openapi.json", get(openapi))
        .route("/schema/{name}", get(schema_one))
        // Real PDFs run tens of MB; axum's 2MB default would reject them.
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(Arc::new(state))
}

async fn healthz() -> Response {
    let body = serde_json::json!({
        "name": "docparse",
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": docparse_core::ir::SCHEMA_VERSION,
    })
    .to_string();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// `GET /schema/{name}` — one output JSON Schema (draft 2020-12), e.g.
/// `/schema/chunk`. The name set matches `docparse schema` and the
/// `components.schemas` keys in `/openapi.json`.
async fn schema_one(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match docparse_core::schema::by_name(&name) {
        Some(schema) => (
            [(header::CONTENT_TYPE, "application/schema+json")],
            serde_json::to_string_pretty(&schema).unwrap_or_default(),
        )
            .into_response(),
        None => err(
            StatusCode::NOT_FOUND,
            &format!("unknown schema: {name} (see GET /openapi.json)"),
        ),
    }
}

/// `GET /openapi.json` — an OpenAPI 3.1 document describing the two endpoints,
/// with every output JSON Schema injected under `components.schemas`. Hand-
/// assembled (the surface is tiny) so the response stays a plain value and we
/// don't pull a second derive framework. The component schemas are generated
/// from the code, so they never drift from the actual output.
async fn openapi() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&openapi_doc()).unwrap_or_default(),
    )
        .into_response()
}

/// Build the OpenAPI 3.1 document. Pure function (no I/O) so a test can assert
/// its shape without binding a socket.
fn openapi_doc() -> serde_json::Value {
    use serde_json::json;
    // OpenAPI 3.1 uses JSON Schema draft 2020-12 — the same dialect schemars
    // emits — so the component schemas drop in directly.
    let mut components = serde_json::Map::new();
    for s in docparse_core::schema::all() {
        components.insert(s.name.to_string(), s.schema);
    }
    // Shared enhancement toggles, expressed once and referenced per endpoint.
    let bool_param = |name: &str, about: &str| {
        json!({
            "name": name, "in": "query", "required": false,
            "schema": { "type": "boolean" }, "description": about
        })
    };
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "docparse",
            "version": env!("CARGO_PKG_VERSION"),
            "x-schema-version": docparse_core::ir::SCHEMA_VERSION,
            "description": "Pure-Rust document parser. Same output as the CLI and MCP faces \
                            (byte-identical for a given input + format)."
        },
        "paths": {
            "/healthz": {
                "get": {
                    "summary": "Liveness + version/schema info.",
                    "responses": { "200": { "description": "Service info JSON." } }
                }
            },
            "/openapi.json": {
                "get": { "summary": "This document.", "responses": { "200": { "description": "OpenAPI 3.1." } } }
            },
            "/schema/{name}": {
                "get": {
                    "summary": "One output JSON Schema.",
                    "parameters": [{
                        "name": "name", "in": "path", "required": true,
                        "schema": { "type": "string",
                                    "enum": ["document", "chunk", "outline", "quality", "profile", "okf-bundle"] }
                    }],
                    "responses": { "200": { "description": "JSON Schema (draft 2020-12)." },
                                   "404": { "description": "Unknown schema name." } }
                }
            },
            "/parse": {
                "post": {
                    "summary": "Parse an uploaded document.",
                    "parameters": [
                        json!({
                            "name": "format", "in": "query", "required": false,
                            "schema": { "type": "string",
                                        "enum": ["json", "markdown", "text", "chunks", "outline", "okf"],
                                        "default": "json" },
                            "description": "Output format. json→document schema, chunks→array of chunk, \
                                            outline→outline schema, okf→application/x-tar bundle."
                        }),
                        bool_param("envelope", "chunks only: wrap the array as {provenance,quality,profile,chunks}."),
                        json!({ "name": "table_format", "in": "query", "required": false,
                                "schema": { "type": "string", "enum": ["tab", "markdown"] },
                                "description": "chunks only: table chunk rendering." }),
                        json!({ "name": "resource_base", "in": "query", "required": false,
                                "schema": { "type": "string" }, "description": "okf only: concept resource URI prefix." }),
                        bool_param("ocr", "OCR scanned pages (needs server --ocr-models)."),
                        bool_param("layout", "Layout-model reading order (PDF only; needs --layout-model)."),
                        bool_param("table_model", "UniRec table structure (needs --unirec-models)."),
                        bool_param("formula_model", "Display formulas to LaTeX (needs --unirec-models)."),
                        bool_param("vlm_describe", "VLM figure captions (needs --vlm-url/--vlm-model)."),
                        bool_param("vlm_tables", "VLM table re-extraction.")
                    ],
                    "requestBody": {
                        "required": true,
                        "content": { "multipart/form-data": { "schema": {
                            "type": "object",
                            "properties": { "file": { "type": "string", "format": "binary" } },
                            "required": ["file"]
                        } } }
                    },
                    "responses": {
                        "200": {
                            "description": "Parsed output (media type depends on format).",
                            "content": {
                                "application/json": { "schema": { "oneOf": [
                                    { "$ref": "#/components/schemas/document" },
                                    { "type": "array", "items": { "$ref": "#/components/schemas/chunk" } },
                                    { "$ref": "#/components/schemas/outline" }
                                ] } },
                                "text/markdown": { "schema": { "type": "string" } },
                                "text/plain": { "schema": { "type": "string" } },
                                "application/x-tar": { "schema": { "type": "string", "format": "binary" } }
                            }
                        },
                        "422": { "description": "Parse failed (unprocessable document)." },
                        "400": { "description": "Bad request (no file field / bad multipart)." }
                    }
                }
            }
        },
        "components": { "schemas": components }
    })
}

async fn parse(
    State(state): State<Arc<crate::EnhanceState>>,
    Query(q): Query<HashMap<String, String>>,
    mut multipart: Multipart,
) -> Response {
    let format = q.get("format").cloned().unwrap_or_else(|| "json".into());
    let flag = |k: &str| matches!(q.get(k).map(String::as_str), Some("1") | Some("true"));
    let images_embedded = q.get("images").map(String::as_str) == Some("embedded");
    // chunks 专用：?envelope=true 把裸 chunk 数组包成 {provenance,quality,profile,chunks}
    // （同 MCP get_chunks），让 RAG 消费方据 quality.flags 决定是否开 OCR/layout。
    let envelope = flag("envelope");
    // chunks 专用：?table_format=markdown 让表格 chunk 出 GitHub 管道表（默认 tab/换行）。
    let table_markdown = q.get("table_format").map(String::as_str) == Some("markdown");
    let opts = crate::EnhanceOpts {
        ocr: flag("ocr"),
        images_embedded,
        layout: flag("layout"),
        table_model: flag("table_model"),
        formula_model: flag("formula_model"),
        vlm_describe: flag("vlm_describe"),
        vlm_tables: flag("vlm_tables"),
    };
    // First field that carries a filename = the document (extension picks the
    // parser backend, same as the CLI).
    let field = loop {
        match multipart.next_field().await {
            Ok(Some(f)) if f.file_name().is_some() => break f,
            Ok(Some(_)) => continue,
            Ok(None) => return err(StatusCode::BAD_REQUEST, "no file field in multipart body"),
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("bad multipart: {e}")),
        }
    };
    let name = sanitize(field.file_name().unwrap_or("upload"));
    let bytes = match field.bytes().await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, &format!("upload read failed: {e}")),
    };

    // Parsers consume &Path, so stage the upload as a temp file for the call.
    let tmp = temp_path(&name);
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("temp write failed: {e}"),
        );
    }
    let started = std::time::Instant::now();

    // OKF is a binary tar bundle, not a text body — handle it before `render`
    // (which returns a String) and return early with application/x-tar.
    if format == "okf" {
        let resource_base = q.get("resource_base").cloned().unwrap_or_default();
        let (tp, tn, st) = (tmp.clone(), name.clone(), state.clone());
        let res =
            tokio::task::spawn_blocking(move || render_okf_tar(&tp, &tn, opts, resource_base, &st))
                .await;
        let elapsed_ms = started.elapsed().as_millis().to_string();
        std::fs::remove_file(&tmp).ok();
        return match res {
            Ok(Ok(tar)) => (
                [
                    (header::CONTENT_TYPE, "application/x-tar".to_string()),
                    (header::HeaderName::from_static("x-docparse-ms"), elapsed_ms),
                ],
                tar,
            )
                .into_response(),
            Ok(Err(e)) => err(StatusCode::UNPROCESSABLE_ENTITY, &format!("{e:#}")),
            Err(e) => err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("task failed: {e}"),
            ),
        };
    }

    let task_path = tmp.clone();
    let task_name = name.clone();
    let rendered = tokio::task::spawn_blocking(move || {
        // Model load (first enhanced request only) and inference are both
        // CPU-bound — they belong on the blocking pool with the parse.
        render(
            &task_path,
            &task_name,
            &format,
            opts,
            envelope,
            table_markdown,
            &state,
        )
    })
    .await;
    let elapsed_ms = started.elapsed().as_millis().to_string();
    std::fs::remove_file(&tmp).ok();

    match rendered {
        Ok(Ok((body, content_type))) => (
            // Timing rides in a header so the body stays byte-identical to
            // the CLI's output (minimal observability, plan N2c).
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::HeaderName::from_static("x-docparse-ms"), elapsed_ms),
            ],
            body,
        )
            .into_response(),
        Ok(Err(e)) => err(StatusCode::UNPROCESSABLE_ENTITY, &format!("{e:#}")),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("task failed: {e}"),
        ),
    }
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

/// Parse + render one document. For the same input/format the body is
/// byte-identical to `docparse <name> -f <format>` (CLI lockstep) — the one
/// REST-only superset is `chunks` + `envelope=true`, which wraps the same
/// chunk array in the MCP `get_chunks` envelope (additive, opt-in).
/// `source_name` replaces the staging temp path in the document's source
/// annotation — clients sent the file, they should see its name, not our temp
/// dir (which would also leak server paths and make responses nondeterministic).
fn render(
    path: &Path,
    source_name: &str,
    format: &str,
    opts: crate::EnhanceOpts,
    envelope: bool,
    table_markdown: bool,
    state: &crate::EnhanceState,
) -> anyhow::Result<(String, &'static str)> {
    let doc = crate::parse_path_with(path, opts.images_embedded)?;
    let mut doc = state.apply(doc, path, opts)?;
    doc.source = source_name.to_string();
    Ok(match format {
        "json" => (output::to_json(&doc)?, "application/json"),
        "markdown" => (output::to_markdown(&doc), "text/markdown; charset=utf-8"),
        "text" => (output::to_text(&doc), "text/plain; charset=utf-8"),
        "chunks" => {
            let chunks = docparse_core::chunk::chunk_document_with(
                &doc,
                docparse_core::chunk::ChunkOptions {
                    table_markdown,
                    ..Default::default()
                },
            );
            let body = if envelope {
                // Same shape as MCP get_chunks: provenance + quality + per-page
                // profile alongside the chunks, so a RAG client can route
                // enhancement (OCR/layout) off quality.flags without a 2nd call.
                serde_json::to_string_pretty(&serde_json::json!({
                    "provenance": serde_json::to_value(&doc.provenance)?,
                    "quality": serde_json::to_value(docparse_core::quality::analyze(&doc))?,
                    "profile": serde_json::to_value(docparse_core::quality::profile(&doc))?,
                    "chunks": serde_json::to_value(&chunks)?,
                }))?
            } else {
                docparse_core::chunk::to_json(&chunks)
            };
            (body, "application/json")
        }
        "outline" => {
            // Structure tree (table of contents). Section ids match chunks'
            // section_id, so a client can outline → then fetch a section's chunks.
            (
                docparse_core::outline::to_json(&docparse_core::outline::build(&doc)),
                "application/json",
            )
        }
        // `okf` is intercepted by the handler (binary tar) before reaching here.
        other => anyhow::bail!("unknown format: {other} (json|markdown|text|chunks|outline|okf)"),
    })
}

/// Parse + build a deterministic OKF tar archive (REST `format=okf`). Separate
/// from `render` because the body is binary (`Vec<u8>`), not text.
fn render_okf_tar(
    path: &Path,
    source_name: &str,
    opts: crate::EnhanceOpts,
    resource_base: String,
    state: &crate::EnhanceState,
) -> anyhow::Result<Vec<u8>> {
    let doc = crate::parse_path_with(path, opts.images_embedded)?;
    let mut doc = state.apply(doc, path, opts)?;
    doc.source = source_name.to_string();
    let okf_opts = crate::okf_options_for(path, resource_base, false);
    Ok(docparse_core::okf::build(&doc, &okf_opts).to_tar())
}

/// Keep only a safe file name (extension included — it selects the backend).
fn sanitize(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload")
        .replace(['/', '\\'], "_")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(name: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("docparse-rest-{}-{n}-{name}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> crate::EnhanceState {
        crate::EnhanceState::new(
            "models/ppocr".into(),
            "models/layout/doclayout_yolo.onnx".into(),
            None,
            None,
        )
    }

    fn temp_html(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(
            &path,
            "<html><body><h1>T</h1><p>Hello rest.</p></body></html>",
        )
        .unwrap();
        path
    }

    #[test]
    fn render_matches_cli_pipeline_and_is_deterministic() {
        let path = temp_html("docparse-rest-test.html");
        let st = test_state();
        let (a, ct) = render(
            &path,
            "up.html",
            "markdown",
            Default::default(),
            false,
            false,
            &st,
        )
        .unwrap();
        let (b, _) = render(
            &path,
            "up.html",
            "markdown",
            Default::default(),
            false,
            false,
            &st,
        )
        .unwrap();
        assert_eq!(a, b, "same input must render byte-identically");
        assert_eq!(ct, "text/markdown; charset=utf-8");
        assert!(a.contains("Hello rest."));
        // Clients see the uploaded name, never the staging temp path.
        assert!(a.contains("up.html") && !a.contains("docparse-rest-test"));
        // Same rendering the CLI does — lockstep modulo the source name.
        let mut doc = crate::parse_path_with(&path, false).unwrap();
        doc.source = "up.html".into();
        assert_eq!(a, output::to_markdown(&doc));
    }

    #[test]
    fn unknown_format_is_an_error() {
        let path = temp_html("docparse-rest-badfmt.html");
        assert!(render(
            &path,
            "x.html",
            "yaml",
            Default::default(),
            false,
            false,
            &test_state()
        )
        .is_err());
    }

    #[test]
    fn chunks_envelope_is_additive_superset() {
        let path = temp_html("docparse-rest-envelope.html");
        let st = test_state();
        // 默认 = 裸数组（与 CLI 字节一致）
        let (bare, ct) = render(
            &path,
            "up.html",
            "chunks",
            Default::default(),
            false,
            false,
            &st,
        )
        .unwrap();
        assert_eq!(ct, "application/json");
        let bare_json: serde_json::Value = serde_json::from_str(&bare).unwrap();
        assert!(bare_json.is_array(), "bare chunks must be a JSON array");

        // envelope=true = {provenance,quality,profile,chunks}，chunks 与裸数组同内容
        let (env, _) = render(
            &path,
            "up.html",
            "chunks",
            Default::default(),
            true,
            false,
            &st,
        )
        .unwrap();
        let env_json: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert!(env_json["provenance"]["parser"].is_string());
        assert!(env_json["quality"]["coverage"].is_number());
        assert!(env_json["profile"].is_array());
        assert_eq!(
            env_json["chunks"], bare_json,
            "envelope.chunks == bare array"
        );
    }

    #[test]
    fn sanitize_strips_directories() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("report.pdf"), "report.pdf");
        assert_eq!(sanitize(""), "upload");
    }

    #[test]
    fn openapi_doc_describes_endpoints_and_embeds_schemas() {
        let doc = openapi_doc();
        assert_eq!(doc["openapi"], "3.1.0");
        // Every output schema is embedded under components and reusable via $ref.
        let comps = &doc["components"]["schemas"];
        for name in [
            "document",
            "chunk",
            "outline",
            "quality",
            "profile",
            "okf-bundle",
        ] {
            assert!(comps.get(name).is_some(), "missing component {name}");
        }
        // The embedded component is the real generated schema, not a stub.
        assert_eq!(
            comps["chunk"],
            docparse_core::schema::by_name("chunk").unwrap()
        );
        // The two endpoints and the contract routes are documented.
        let paths = &doc["paths"];
        assert!(paths["/parse"]["post"].is_object());
        assert!(paths["/schema/{name}"]["get"].is_object());
        // A consumer can read /parse's response media types.
        assert!(
            paths["/parse"]["post"]["responses"]["200"]["content"]["application/x-tar"].is_object()
        );
    }
}
