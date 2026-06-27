//! Machine-readable output contract — JSON Schema (draft 2020-12) for every
//! agent-facing output, generated from the same serde types that produce the
//! JSON (one source of truth; see the `#[cfg_attr(feature = "schema", …)]`
//! derives on [`crate::ir`], [`crate::chunk`], [`crate::outline`],
//! [`crate::quality`]).
//!
//! Why: external agent/RAG projects shouldn't have to read prose docs and hand-
//! transcribe field names. With these schemas they can codegen typed clients
//! (`datamodel-codegen`, `quicktype`) and validate responses. The CLI exposes
//! them via `docparse schema`, REST via `GET /openapi.json` + `/schema/{name}`,
//! MCP via `resources/*` and per-tool `outputSchema`.
//!
//! Drift is impossible to ship: the committed `schemas/*.json` are checked
//! against [`all`] by a golden test (`docparse schema --write` regenerates).
//!
//! Only compiled under the `schema` feature.

use serde_json::{json, Value};

/// One named schema: a stable key (used in file names, REST paths, and MCP
/// resource URIs) and its JSON Schema document.
pub struct NamedSchema {
    pub name: &'static str,
    /// One-line human description of what output this schema describes.
    pub title: &'static str,
    pub schema: Value,
}

/// Generate a schema document for `T` as a plain JSON value.
fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T))
        .expect("a derived JSON Schema always serializes to a Value")
}

/// All agent-facing output schemas, keyed by stable name. The key set is the
/// contract surface: `document` (`-f json`), `chunk` (one element of
/// `-f chunks`), `outline` (`-f outline`), `quality` (the quality report),
/// `profile` (one per-page profile), and `okf-bundle` (the `export_okf` result).
pub fn all() -> Vec<NamedSchema> {
    vec![
        NamedSchema {
            name: "document",
            title: "Parsed document (-f json): pages of positioned elements with provenance.",
            schema: schema_for::<crate::ir::Document>(),
        },
        NamedSchema {
            name: "chunk",
            title: "One retrieval chunk (element of -f chunks): text + citable page/bbox + heading breadcrumb.",
            schema: schema_for::<crate::chunk::Chunk>(),
        },
        NamedSchema {
            name: "outline",
            title: "Document structure tree (-f outline): nested citable sections.",
            schema: schema_for::<crate::outline::Section>(),
        },
        NamedSchema {
            name: "quality",
            title: "Quality report: coverage, garble ratio, and routing flags.",
            schema: schema_for::<crate::quality::QualityReport>(),
        },
        NamedSchema {
            name: "profile",
            title: "Per-page complexity profile.",
            schema: schema_for::<crate::quality::PageProfile>(),
        },
        NamedSchema {
            name: "okf-bundle",
            title: "Open Knowledge Format bundle (export_okf result): version + inline concept files.",
            schema: okf_bundle_schema(),
        },
    ]
}

/// Look up one schema by name (REST `/schema/{name}`, MCP resource read).
pub fn by_name(name: &str) -> Option<Value> {
    all().into_iter().find(|s| s.name == name).map(|s| s.schema)
}

/// The OKF bundle response is assembled by hand (not a single serde type — the
/// `export_okf` tool builds it as JSON), so its schema is hand-written to match
/// [`crate::okf`] / `cli::mcp::tool_export_okf`. Kept tiny and stable.
fn okf_bundle_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "OkfBundle",
        "description": "Open Knowledge Format (OKF v0.1) bundle: Markdown + YAML-frontmatter \
                        concept files mirroring the structure tree.",
        "type": "object",
        "required": ["okf_version", "files"],
        "properties": {
            "okf_version": { "type": "string", "const": "0.1" },
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Bundle-relative path, e.g. \"index.md\" or \"03-methods/04-model.md\"."
                        },
                        "content": {
                            "type": "string",
                            "description": "Markdown body with YAML frontmatter (type/resource/level/title)."
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_schemas_are_objects_with_expected_keys() {
        let names: Vec<&str> = all().iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "document",
                "chunk",
                "outline",
                "quality",
                "profile",
                "okf-bundle"
            ]
        );
        for s in all() {
            assert!(s.schema.is_object(), "{} schema must be an object", s.name);
        }
    }

    #[test]
    fn chunk_schema_carries_citation_fields() {
        let chunk = by_name("chunk").expect("chunk schema");
        let props = &chunk["properties"];
        for field in ["page", "bbox", "heading_path", "section_id", "char_len"] {
            assert!(
                props.get(field).is_some(),
                "chunk schema missing `{field}`: {props}"
            );
        }
    }

    #[test]
    fn image_data_is_not_in_schema() {
        // `data: Vec<u8>` is `#[serde(skip)]` — it must not leak into the
        // contract (it's never serialized).
        let doc = by_name("document").expect("document schema");
        let defs = &doc["$defs"];
        let image = &defs["ImageChunk"]["properties"];
        assert!(
            image.get("data").is_none(),
            "ImageChunk.data must be skipped"
        );
        assert!(image.get("bbox").is_some());
    }
}
