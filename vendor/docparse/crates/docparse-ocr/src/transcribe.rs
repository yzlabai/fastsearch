//! `--transcribe-model`: full-page re-recognition for layouts the
//! deterministic pipeline can't order (G8d, the only on-record fix for the
//! CJK design-page gap — G2 proved region-level layout models can't repair
//! the in-region micro order, and three geometric routing criteria failed).
//!
//! Pipeline per page: render (hayro) → DocLayout-YOLO regions → order
//! regions with the text XY-cut → UniRec reads each text-bearing region →
//! REPLACE the page's text chunks with one positioned chunk per region
//! (`group` carries the region's reading rank, `source:
//! "transcribe:unirec-0.1b"`). Tables and images survive untouched; `title`
//! regions get a heading-sized font so downstream classification fires.
//!
//! Positions become REGION-level (line-level geometry is the price of
//! transcription — documented, and the reason this is opt-in). A degradation
//! gate keeps the original page whenever transcription recovers materially
//! less text than the deterministic parse already had.

use crate::layout::{region_rank, LayoutModel, Region};
use crate::table_model::crop_region;
use crate::unirec::UniRec;
use anyhow::Result;
use docparse_core::ir::{Document, Element, TextChunk};

/// Detection confidence floor.
const SCORE_MIN: f32 = 0.30;
/// Render scale (pixels per PDF point).
const RENDER_SCALE: f32 = 3.0;
/// Per-region generation cap (a dense text region runs a few hundred tokens).
const MAX_TOKENS: usize = 1200;
/// Degradation gate: keep the original page when transcription yields less
/// than this fraction of the deterministic text volume.
const MIN_CHAR_RATIO: f32 = 0.5;

/// Transcribe every page. Returns the number of pages replaced.
pub fn transcribe_pages(
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
                eprintln!("transcribe: render failed on page {}: {e:#}", page.number);
                continue;
            }
        };
        let regions: Vec<Region> =
            match layout.detect(&rgb, w as usize, h as usize, RENDER_SCALE, page.height) {
                Ok(r) => r
                    .into_iter()
                    .filter(|r| r.kind.is_textual() && r.score >= SCORE_MIN)
                    .collect(),
                Err(e) => {
                    eprintln!("transcribe: layout failed on page {}: {e:#}", page.number);
                    continue;
                }
            };
        if regions.is_empty() {
            continue;
        }
        let rank = region_rank(page.number, &regions);

        let mut new_chunks: Vec<TextChunk> = Vec::new();
        for (i, region) in regions.iter().enumerate() {
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
                    let text = text.trim().to_string();
                    if text.is_empty() || crate::unirec::looks_degenerate(&text) {
                        continue; // out-of-domain repetition loop — reject
                    }
                    new_chunks.push(TextChunk {
                        text,
                        bbox: region.bbox,
                        font_size: if region.kind.is_title() { 16.0 } else { 10.0 },
                        font: None,
                        page: page.number,
                        confidence: 0.85,
                        bold: false,
                        hidden: false,
                        source: Some("transcribe:unirec-0.1b".to_string()),
                        group: Some(rank[i]),
                        tag: None,
                    });
                }
                Err(e) => eprintln!(
                    "transcribe: region inference failed on page {}: {e:#}",
                    page.number
                ),
            }
        }

        // Degradation gate: only swap when transcription holds its own
        // against what the deterministic parse already extracted.
        let old_chars: usize = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) if !t.hidden => Some(t.text.chars().count()),
                _ => None,
            })
            .sum();
        let new_chars: usize = new_chunks.iter().map(|c| c.text.chars().count()).sum();
        if new_chunks.is_empty()
            || (old_chars > 0 && (new_chars as f32) < (old_chars as f32) * MIN_CHAR_RATIO)
        {
            eprintln!(
                "transcribe: page {} kept deterministic text ({} -> {} chars)",
                page.number, old_chars, new_chars
            );
            continue;
        }

        page.elements.retain(|e| !matches!(e, Element::Text(_)));
        page.elements
            .extend(new_chunks.into_iter().map(Element::Text));
        replaced += 1;
    }
    Ok(replaced)
}
