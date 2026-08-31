//! Shared, lightweight ingestion adapter boundary.
//!
//! The profile type is always available. The docparse implementation is compiled only when the
//! `parse` feature is enabled, so server/engine/default CLI dependency graphs remain parser-free.

use anyhow::Result;
use fastsearch_core::Chunk;

/// A traceable chunking profile shared by text and document ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkProfile {
    name: String,
    version: u32,
    target_chars: usize,
    overlap_chars: usize,
    table_markdown: bool,
}

impl ChunkProfile {
    pub fn new(
        name: impl Into<String>,
        version: u32,
        target_chars: usize,
        overlap_chars: usize,
        table_markdown: bool,
    ) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            anyhow::bail!("chunk profile name must not be empty");
        }
        if version == 0 {
            anyhow::bail!("chunk profile version must be greater than zero");
        }
        if target_chars == 0 {
            anyhow::bail!("chunk target must be greater than zero");
        }
        if overlap_chars >= target_chars {
            anyhow::bail!(
                "chunk overlap ({overlap_chars}) must be smaller than target ({target_chars})"
            );
        }
        Ok(Self {
            name,
            version,
            target_chars,
            overlap_chars,
            table_markdown,
        })
    }

    pub fn text_default() -> Self {
        Self::new("fastsearch-text", 1, 900, 0, false).expect("valid built-in text profile")
    }

    pub fn docparse_default() -> Self {
        Self::new("docparse", 1, 800, 0, false).expect("valid built-in docparse profile")
    }

    pub fn target_chars(&self) -> usize {
        self.target_chars
    }

    pub fn overlap_chars(&self) -> usize {
        self.overlap_chars
    }

    pub fn table_markdown(&self) -> bool {
        self.table_markdown
    }

    pub fn attach_to(&self, chunk: &mut Chunk, chunker: &str) {
        chunk.metadata.insert(
            "chunking".into(),
            serde_json::json!({
                "chunker": chunker,
                "profile": self.name,
                "version": self.version,
                "target_chars": self.target_chars,
                "overlap_chars": self.overlap_chars,
                "table_markdown": self.table_markdown,
            }),
        );
    }
}

#[cfg(feature = "parse")]
mod parse;
#[cfg(feature = "parse")]
pub use parse::{chunks_for_file, from_docparse_chunk, Enhancements, ImageBytes, ParseOptions};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_rejects_invalid_boundaries() {
        assert!(ChunkProfile::new("", 1, 100, 0, false).is_err());
        assert!(ChunkProfile::new("p", 0, 100, 0, false).is_err());
        assert!(ChunkProfile::new("p", 1, 0, 0, false).is_err());
        assert!(ChunkProfile::new("p", 1, 100, 100, false).is_err());
    }
}
