//! `docparse schema` — emit the machine-readable output contract.
//!
//! JSON Schema (draft 2020-12) for every agent-facing output, generated from the
//! serde types in `docparse-core` (see [`docparse_core::schema`]). External
//! agent/RAG projects use these to codegen typed clients or validate responses;
//! the same schemas back REST `/openapi.json` + `/schema/{name}` and the MCP
//! per-tool `outputSchema`.
//!
//! The committed `schemas/*.json` are the published copy; a golden test
//! ([`tests::committed_schemas_are_current`]) fails if they drift from the code,
//! and `docparse schema --write` regenerates them.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// Canonical on-disk / on-wire form for one schema: pretty JSON + trailing
/// newline. Centralized so `--write` and the golden test agree byte-for-byte.
fn render(schema: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(schema).expect("schema serializes");
    s.push('\n');
    s
}

/// Repo `schemas/` directory (sibling of `crates/`), relative to this crate.
fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

/// `docparse schema [--name N] [--write]`.
pub fn run(name: Option<&str>, write: bool) -> anyhow::Result<()> {
    if write {
        let dir = schemas_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        for s in docparse_core::schema::all() {
            let path = dir.join(format!("{}.json", s.name));
            std::fs::write(&path, render(&s.schema))
                .with_context(|| format!("write {}", path.display()))?;
            eprintln!("wrote {}", path.display());
        }
        return Ok(());
    }
    match name {
        Some(n) => {
            let schema = docparse_core::schema::by_name(n).ok_or_else(|| {
                anyhow::anyhow!("unknown schema: {n} (see `docparse schema` for the set)")
            })?;
            print!("{}", render(&schema));
        }
        None => {
            // A single object keyed by name — convenient for embedding/inspection.
            let mut map = serde_json::Map::new();
            for s in docparse_core::schema::all() {
                map.insert(s.name.to_string(), s.schema);
            }
            print!("{}", render(&serde_json::Value::Object(map)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed `schemas/*.json` must equal what the code generates today.
    /// If this fails, the output contract changed: run `docparse schema --write`
    /// and commit the result (after confirming the change is intended).
    #[test]
    fn committed_schemas_are_current() {
        let dir = schemas_dir();
        for s in docparse_core::schema::all() {
            let path = dir.join(format!("{}.json", s.name));
            let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                panic!("missing {} — run `docparse schema --write`", path.display())
            });
            assert_eq!(
                on_disk,
                render(&s.schema),
                "{} is stale — run `docparse schema --write` and commit",
                path.display()
            );
        }
    }

    #[test]
    fn unknown_name_is_error() {
        assert!(run(Some("nope"), false).is_err());
    }
}
