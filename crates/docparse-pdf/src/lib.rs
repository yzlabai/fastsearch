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
        // Read bytes ourselves so the path and in-memory entries share the same
        // tolerant loader (CNKI/万方 PDFs need a repair pass; see load_tolerant).
        let bytes = std::fs::read(path)?;
        let mut out = self.parse_document(load_tolerant(&bytes)?)?;
        out.source = path.display().to_string();
        Ok(out)
    }
}

impl PdfParser {
    /// Parse an in-memory PDF (REST uploads, fuzzing) — same pipeline as the
    /// path-based entry, minus the file read.
    pub fn parse_bytes(&self, bytes: &[u8]) -> anyhow::Result<Document> {
        self.parse_document(load_tolerant(bytes)?)
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

/// Load a PDF, retrying with a byte-level repair pass if the strict parse fails.
///
/// CNKI / 万方 (common Chinese journal/thesis sources) write the cross-reference
/// section as `xref 0 67` — the subsection header on the SAME line as the `xref`
/// keyword. The spec puts it on the next line, and lopdf's reader rejects the
/// variant with `InvalidTrailer` (MuPDF/Acrobat tolerate it). We only pay the
/// repair cost on the error path, so well-formed PDFs are untouched.
fn load_tolerant(bytes: &[u8]) -> anyhow::Result<PdfDocument> {
    match PdfDocument::load_mem(bytes) {
        Ok(doc) => Ok(doc),
        Err(first_err) => match repair_xref_keyword(bytes) {
            Some(fixed) => PdfDocument::load_mem(&fixed).map_err(|_| first_err.into()),
            None => Err(first_err.into()),
        },
    }
}

/// Insert an EOL after a line-leading `xref` keyword that is followed by the
/// subsection header on the same line (`xref 0 67` → `xref\r\n0 67`). Offset-safe:
/// xref entries point at objects *before* the table, and `startxref` points at
/// the `xref` keyword itself — both unaffected by inserting after the keyword.
/// Returns `None` when nothing matched (caller keeps the original bytes/error).
fn repair_xref_keyword(bytes: &[u8]) -> Option<Vec<u8>> {
    const KW: &[u8] = b"xref";
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    let mut changed = false;
    while i < bytes.len() {
        let at_line_start = i == 0 || bytes[i - 1] == b'\n' || bytes[i - 1] == b'\r';
        // `xref` (not `startxref`, which the line-start check already excludes)
        // followed by horizontal whitespace then a digit = the malformed header.
        if at_line_start && bytes[i..].starts_with(KW) {
            let after = i + KW.len();
            let mut j = after;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j > after && j < bytes.len() && bytes[j].is_ascii_digit() {
                out.extend_from_slice(KW);
                out.extend_from_slice(b"\r\n");
                i = j; // drop the horizontal whitespace, keep the digits
                changed = true;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    if changed {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod repair_tests {
    use super::repair_xref_keyword;

    #[test]
    fn normalizes_same_line_xref_header() {
        let got = repair_xref_keyword(b"...\r\nxref 0 67\r\n0000 n\r\n").unwrap();
        assert!(
            got.windows(8).any(|w| w == b"xref\r\n0 "),
            "got: {}",
            String::from_utf8_lossy(&got)
        );
    }

    #[test]
    fn leaves_conforming_xref_untouched() {
        // `xref` already followed by EOL → no change → None.
        assert!(repair_xref_keyword(b"...\r\nxref\r\n0 67\r\n").is_none());
        // `startxref 123` is not line-leading `xref`+ws+digit at the keyword.
        assert!(repair_xref_keyword(b"startxref\r\n834254\r\n%%EOF").is_none());
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
