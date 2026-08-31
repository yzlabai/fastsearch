//! CLI facade for the shared ingestion adapter.

use anyhow::Result;
use fastsearch_core::Chunk;
use std::path::PathBuf;

pub use fastsearch_ingest_adapter::{from_docparse_chunk, ImageBytes};

/// `fastsearch ingest <file>` options: local parsing followed by REST `/v1/index`.
pub struct IngestOpts {
    pub file: PathBuf,
    pub server: Option<String>,
    pub key: Option<String>,
    pub collection: String,
    pub doc_id: String,
    pub tenant: Option<String>,
    pub acl: Vec<String>,
    pub images: ImageBytes,
    pub chunk_profile: crate::ChunkProfile,
}

pub fn chunks_for_file(opts: &IngestOpts) -> Result<Vec<Chunk>> {
    fastsearch_ingest_adapter::chunks_for_file(&fastsearch_ingest_adapter::ParseOptions {
        file: opts.file.clone(),
        doc_id: opts.doc_id.clone(),
        tenant: opts.tenant.clone(),
        acl: opts.acl.clone(),
        images: opts.images,
        chunk_profile: opts.chunk_profile.clone(),
        // Preserve the legacy CLI behavior: compiled enhancements activate only when their
        // existing runtime model/service environment is configured.
        enhancements: fastsearch_ingest_adapter::Enhancements {
            ocr: true,
            tables: true,
            vlm: true,
        },
    })
}

pub fn cmd_ingest(opts: &IngestOpts) -> Result<usize> {
    let chunks = chunks_for_file(opts)?;
    let client = crate::Client::new(opts.server.clone(), opts.key.clone());
    let store_media = match opts.images {
        ImageBytes::Object => Some(crate::StoreMedia::Object),
        ImageBytes::Inline => Some(crate::StoreMedia::Inline),
        ImageBytes::None => None,
    };
    crate::post_index(
        &client,
        &opts.collection,
        &opts.doc_id,
        store_media,
        &chunks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_modes_keep_existing_store_media_contract() {
        let map = |mode| match mode {
            ImageBytes::Object => Some(crate::StoreMedia::Object),
            ImageBytes::Inline => Some(crate::StoreMedia::Inline),
            ImageBytes::None => None,
        };
        assert!(matches!(
            map(ImageBytes::Object),
            Some(crate::StoreMedia::Object)
        ));
        assert!(matches!(
            map(ImageBytes::Inline),
            Some(crate::StoreMedia::Inline)
        ));
        assert!(map(ImageBytes::None).is_none());
    }
}
