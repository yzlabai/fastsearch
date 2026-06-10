//! Pure-Rust PDF backend.
//!
//! Pipeline: `lopdf` loads the COS object model and decodes page content
//! streams; [`interpreter`] walks the operators to emit positioned text.
//! Page content is collected sequentially (cheap), then interpreted in
//! parallel with Rayon (the CPU-heavy part) — mirroring opendataloader-pdf's
//! per-page `ForkJoinPool` model.

mod cmap;
mod encoding;
mod encoding_tables;
mod font;
mod images;
mod interpreter;
mod matrix;
mod stdmetrics;

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use interpreter::{interpret, PageInput};
use lopdf::{Document as PdfDocument, Object, ObjectId};
use rayon::prelude::*;
use std::path::Path;

/// Default page size (US Letter) when a MediaBox can't be resolved.
const DEFAULT_PAGE: (f32, f32) = (612.0, 792.0);

pub struct PdfParser;

impl DocumentParser for PdfParser {
    fn name(&self) -> &'static str {
        "pdf"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let doc = PdfDocument::load(path)?;

        // Resource guard (N5b): refuse a pathological page count before doing
        // any per-page work.
        let pages_map = doc.get_pages();
        docparse_core::limits::check_page_count(pages_map.len())?;

        // 1) Collect per-page inputs sequentially (I/O + decompression).
        let mut inputs: Vec<PageInput> = Vec::new();
        for (number, page_id) in pages_map {
            let content = doc.get_page_content(page_id).unwrap_or_default();
            let (width, height) = page_dimensions(&doc, page_id);
            let fonts = font::build_page_fonts(&doc, page_id);
            let images = images::build_page_images(&doc, page_id);
            inputs.push(PageInput {
                number: number as usize,
                width,
                height,
                content,
                fonts,
                images,
            });
        }

        // 2) Interpret content streams in parallel (CPU-bound, no shared state).
        let mut pages: Vec<_> = inputs.par_iter().map(interpret).collect();
        pages.sort_by_key(|p| p.number);

        Ok(Document {
            source: path.display().to_string(),
            provenance: Some(Provenance::new("pdf", env!("CARGO_PKG_VERSION"))),
            pages,
        })
    }
}

/// Resolve a page's MediaBox to (width, height). TODO: walk the Pages tree for
/// inherited MediaBox; for now fall back to US Letter when absent on the page.
fn page_dimensions(doc: &PdfDocument, page_id: ObjectId) -> (f32, f32) {
    if let Ok(dict) = doc.get_dictionary(page_id) {
        if let Ok(Object::Array(mb)) = dict.get(b"MediaBox") {
            let v: Vec<f32> = mb
                .iter()
                .map(|o| match o {
                    Object::Integer(i) => *i as f32,
                    Object::Real(r) => *r,
                    _ => 0.0,
                })
                .collect();
            if v.len() == 4 {
                return ((v[2] - v[0]).abs(), (v[3] - v[1]).abs());
            }
        }
    }
    DEFAULT_PAGE
}
