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
mod structure;

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use interpreter::{interpret, PageInput};
use lopdf::{Document as PdfDocument, Object, ObjectId};
use rayon::prelude::*;
use std::path::Path;

/// Default page size (US Letter) when a MediaBox can't be resolved.
const DEFAULT_PAGE: (f32, f32) = (612.0, 792.0);

#[derive(Default)]
pub struct PdfParser {
    /// Decode every embedded image's pixels (≥16px a side), not just
    /// page-covering scan candidates — set by the image-export path; costs
    /// memory on image-heavy documents, so off by default.
    pub decode_images: bool,
}

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
        let mut out = self.parse_document(doc)?;
        out.source = path.display().to_string();
        Ok(out)
    }
}

impl PdfParser {
    /// Parse an in-memory PDF (REST uploads, fuzzing) — same pipeline as the
    /// path-based entry, minus the file read.
    pub fn parse_bytes(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        self.parse_document(PdfDocument::load_mem(bytes)?)
    }

    fn parse_document(&self, doc: PdfDocument) -> anyhow::Result<Document> {
        // Resource guard (N5b): refuse a pathological page count before doing
        // any per-page work.
        let pages_map = doc.get_pages();
        docparse_core::limits::check_page_count(pages_map.len())?;

        // Tagged-PDF structure tree (G9a): author-declared roles + reading
        // order, applied via MCIDs; empty on untagged documents.
        let mut page_tags = structure::build_page_tags(&doc);

        // 1) Collect per-page inputs sequentially (I/O + decompression).
        let mut inputs: Vec<PageInput> = Vec::new();
        for (number, page_id) in pages_map {
            let content = doc.get_page_content(page_id).unwrap_or_default();
            let (origin_x, origin_y, width, height) = page_box(&doc, page_id);
            let rotate = page_rotation(&doc, page_id);
            let fonts = font::build_page_fonts(&doc, page_id);
            let images = images::build_page_images(&doc, page_id);
            let forms = images::build_page_forms(&doc, page_id);
            let tags = page_tags.remove(&(number as usize)).unwrap_or_default();
            inputs.push(PageInput {
                number: number as usize,
                width,
                height,
                origin: (origin_x, origin_y),
                rotate,
                content,
                fonts,
                images,
                forms,
                tags,
                decode_images: self.decode_images,
            });
        }

        // 2) Interpret content streams in parallel (CPU-bound, no shared state).
        let mut pages: Vec<_> = inputs.par_iter().map(interpret).collect();
        pages.sort_by_key(|p| p.number);

        Ok(Document {
            source: "<pdf>".to_string(),
            provenance: Some(Provenance::new("pdf", env!("CARGO_PKG_VERSION"))),
            pages,
        })
    }
}

/// Resolve a page attribute, walking the Pages tree upward — MediaBox and
/// Rotate are inheritable (PDF 32000-1 §7.7.3.4). Depth-capped against
/// malformed Parent cycles; the returned object is already dereferenced.
fn inherited_attr(doc: &PdfDocument, page_id: ObjectId, key: &[u8]) -> Option<Object> {
    let mut id = page_id;
    for _ in 0..32 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(v) = dict.get(key) {
            return doc.dereference(v).ok().map(|(_, o)| o.clone());
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(p)) => id = *p,
            _ => return None,
        }
    }
    None
}

/// Resolve a page's MediaBox (inherited) to (origin_x, origin_y, width,
/// height); US Letter at origin 0 when absent — a fallback, not data
/// (flagged here rather than silently sized).
fn page_box(doc: &PdfDocument, page_id: ObjectId) -> (f32, f32, f32, f32) {
    if let Some(Object::Array(mb)) = inherited_attr(doc, page_id, b"MediaBox") {
        let v: Vec<f32> = mb
            .iter()
            .map(|o| match o {
                Object::Integer(i) => *i as f32,
                Object::Real(r) => *r,
                _ => 0.0,
            })
            .collect();
        if v.len() == 4 {
            return (
                v[0].min(v[2]),
                v[1].min(v[3]),
                (v[2] - v[0]).abs(),
                (v[3] - v[1]).abs(),
            );
        }
    }
    (0.0, 0.0, DEFAULT_PAGE.0, DEFAULT_PAGE.1)
}

/// A page's /Rotate (inherited) as quarter-turns clockwise (0..=3). The spec
/// requires an integer multiple of 90, but Real values occur in the wild;
/// anything else is ignored.
fn page_rotation(doc: &PdfDocument, page_id: ObjectId) -> u8 {
    let r = match inherited_attr(doc, page_id, b"Rotate") {
        Some(Object::Integer(r)) => r,
        Some(Object::Real(r)) => r as i64,
        _ => return 0,
    };
    let r = ((r % 360) + 360) % 360;
    if r % 90 == 0 {
        (r / 90) as u8
    } else {
        0
    }
}
