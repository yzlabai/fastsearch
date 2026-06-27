//! The trait every format backend implements.

use crate::ir::Document;
use std::path::Path;

/// A document parser for one or more file formats.
///
/// Implementations must be `Send + Sync` so the CLI can hold a registry of
/// boxed parsers and run them across files in parallel.
pub trait DocumentParser: Send + Sync {
    /// Short identifier, e.g. `"pdf"`.
    fn name(&self) -> &'static str;

    /// Whether this parser can handle `path` (typically by extension/magic).
    fn supports(&self, path: &Path) -> bool;

    /// Parse `path` into the shared [`Document`] IR.
    fn parse(&self, path: &Path) -> anyhow::Result<Document>;
}
