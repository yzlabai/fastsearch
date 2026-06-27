//! `--formula-model` (G8c via the UniRec pipeline): formula → LaTeX.
//!
//! Born-digital formulas are vector glyph soup in the text layer — symbols
//! arrive in paint order with sub/superscripts detached ("E = mc 2"), which
//! is the worst content in the document for RAG. The embedded DocLayout-YOLO
//! (G2) already detects `isolate_formula` regions; this task renders each
//! one and lets UniRec emit the LaTeX.
//!
//! Replacement is the contract of enhancement flags (as with
//! `--table-model`, which replaces grid rows): text chunks inside an
//! accepted formula region are REPLACED by one LaTeX chunk tagged
//! `Formula` with `source: "formula:unirec-0.1b"`. Without the flag the
//! deterministic output is untouched; a rejected model answer keeps the
//! original chunks.

use crate::layout::LayoutModel;
use crate::table_model::crop_region;
use crate::unirec::UniRec;
use anyhow::Result;
use docparse_core::ir::{Document, Element, TextChunk};

/// Detection confidence floor (same spirit as the layout enhancer's gate).
const SCORE_MIN: f32 = 0.35;
/// Render scale for formula crops (pixels per PDF point).
const RENDER_SCALE: f32 = 3.0;
/// Formulas are short; runaway generation is cut early.
const MAX_TOKENS: usize = 400;

/// Detect formula regions on every page and replace their glyph soup with
/// model LaTeX. Returns the number of formulas replaced.
pub fn enhance_formulas(
    doc: &mut Document,
    pdf_bytes: Vec<u8>,
    layout: &LayoutModel,
    model: &UniRec,
) -> Result<usize> {
    let raster = docparse_raster::Rasterizer::new(pdf_bytes)?;
    let mut replaced = 0usize;

    for page in &mut doc.pages {
        let idx = page.number.saturating_sub(1);
        let (w, h, rgb) = match raster.render_rgb(idx, RENDER_SCALE) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "formula-model: render failed on page {}: {e:#}",
                    page.number
                );
                continue;
            }
        };
        let regions = match layout.detect(&rgb, w as usize, h as usize, RENDER_SCALE, page.height) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "formula-model: layout failed on page {}: {e:#}",
                    page.number
                );
                continue;
            }
        };
        for region in regions
            .iter()
            .filter(|r| r.kind.is_formula_block() && r.score >= SCORE_MIN)
        {
            let Some((cw, ch, crop)) = crop_region(
                &rgb,
                w as usize,
                h as usize,
                &region.bbox,
                page.height,
                RENDER_SCALE,
            ) else {
                continue;
            };
            match model.recognize(&crop, cw, ch, MAX_TOKENS) {
                Ok(text) => {
                    let Some(latex) = usable_latex(&text) else {
                        eprintln!(
                            "formula-model: page {} answer not usable; keeping original text",
                            page.number
                        );
                        continue;
                    };
                    if apply_formula(page, &region.bbox, &latex) {
                        replaced += 1;
                    }
                }
                Err(e) => eprintln!(
                    "formula-model: inference failed on page {}: {e:#}",
                    page.number
                ),
            }
        }
    }
    Ok(replaced)
}

/// A usable formula answer: non-empty, single-expression-sized, and not some
/// other content type the model recognized instead (a table, a paragraph).
fn usable_latex(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.len() > 2000 || t.contains("<table") || crate::unirec::looks_degenerate(t)
    {
        return None;
    }
    // A formula answer should look like math, not prose: require at least
    // one TeX command, operator, or math punctuation.
    if !t.contains('\\') && !t.contains(['=', '^', '_', '∑', '∫']) {
        return None;
    }
    Some(t.to_string())
}

/// Replace the text chunks whose center lies inside `bbox` with one LaTeX
/// chunk. Pure (no model/IO) — unit-tested. Returns false when the region
/// holds no text at all (nothing to supersede: the formula is an image-only
/// region, still worth injecting — handled by inserting regardless).
pub(crate) fn apply_formula(
    page: &mut docparse_core::ir::Page,
    bbox: &docparse_core::ir::BBox,
    latex: &str,
) -> bool {
    page.elements.retain(|e| match e {
        Element::Text(t) => {
            let cx = (t.bbox.x0 + t.bbox.x1) / 2.0;
            let cy = (t.bbox.y0 + t.bbox.y1) / 2.0;
            !(cx >= bbox.x0 && cx <= bbox.x1 && cy >= bbox.y0 && cy <= bbox.y1)
        }
        _ => true,
    });
    page.elements.push(Element::Text(TextChunk {
        text: latex.to_string(),
        bbox: *bbox,
        font_size: 10.0,
        font: None,
        page: page.number,
        confidence: 0.8,
        bold: false,
        hidden: false,
        source: Some("formula:unirec-0.1b".to_string()),
        group: None,
        tag: Some("Formula".to_string()),
    }));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::{BBox, Page};

    fn chunk(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> Element {
        Element::Text(TextChunk {
            text: text.into(),
            bbox: BBox { x0, y0, x1, y1 },
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        })
    }

    #[test]
    fn replaces_only_chunks_inside_region() {
        let mut page = Page {
            number: 1,
            width: 612.0,
            height: 792.0,
            elements: vec![
                chunk("E", 100.0, 500.0, 110.0, 512.0),
                chunk("= mc", 112.0, 500.0, 140.0, 512.0),
                chunk("2", 141.0, 506.0, 147.0, 514.0),
                chunk("Body text far away", 100.0, 100.0, 300.0, 112.0),
            ],
        };
        let region = BBox {
            x0: 95.0,
            y0: 495.0,
            x1: 150.0,
            y1: 518.0,
        };
        assert!(apply_formula(&mut page, &region, "E = mc^{2}"));
        let texts: Vec<&str> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Body text far away", "E = mc^{2}"]);
        let Element::Text(f) = page.elements.last().unwrap() else {
            unreachable!()
        };
        assert_eq!(f.tag.as_deref(), Some("Formula"));
        assert_eq!(f.source.as_deref(), Some("formula:unirec-0.1b"));
    }

    #[test]
    fn latex_gate() {
        assert!(usable_latex("E = mc^{2}").is_some());
        assert!(usable_latex("\\frac{a}{b}").is_some());
        assert!(usable_latex("").is_none());
        assert!(usable_latex("just some prose words here").is_none());
        assert!(usable_latex("<table><tr><td>1</td></tr></table>").is_none());
    }
}
