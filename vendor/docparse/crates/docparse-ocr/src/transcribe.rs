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
use crate::table_model::{crop_region, Crop};
use anyhow::Result;
use docparse_core::ir::{BBox, Document, Element, Page, TextChunk};
use docparse_core::region_reader::{RegionImage, RegionReader};

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
    reader: &dyn RegionReader,
) -> Result<usize> {
    let source = format!("transcribe:{}", reader.source_tag());
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

        // Crop everything first, read the page as one batch, then assemble:
        // a network-bound reader pipelines the whole page instead of stalling
        // region by region (an in-process one falls through to the serial
        // default and behaves exactly as before).
        //
        // The trade is memory: crops for a whole page are now live at once
        // rather than one at a time. Regions tile the page, so this is bounded
        // by roughly the page render itself (~13MB at scale 3 on Letter) —
        // paid once per page, and freed before the next.
        let cropped: Vec<(usize, Crop)> = regions
            .iter()
            .enumerate()
            .filter_map(|(i, region)| {
                crop_region(
                    &rgb,
                    w as usize,
                    h as usize,
                    &region.bbox,
                    page.height,
                    RENDER_SCALE,
                )
                .map(|c| (i, c))
            })
            .collect();
        if cropped.is_empty() {
            continue;
        }
        let imgs: Vec<RegionImage<'_>> = cropped
            .iter()
            .map(|(_, (cw, ch, buf))| RegionImage::new(buf, *cw, *ch))
            .collect();

        let reads: Vec<RegionRead> = cropped
            .iter()
            .zip(reader.read_batch(&imgs, MAX_TOKENS))
            .map(|((i, _), out)| RegionRead {
                bbox: regions[*i].bbox,
                is_title: regions[*i].kind.is_title(),
                rank: rank[*i],
                text: match out {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!(
                            "transcribe: region inference failed on page {}: {e:#}",
                            page.number
                        );
                        None
                    }
                },
            })
            .collect();

        let Some(new_chunks) = build_replacement(page, &reads, &source) else {
            continue;
        };
        page.elements.retain(|e| !matches!(e, Element::Text(_)));
        page.elements
            .extend(new_chunks.into_iter().map(Element::Text));
        replaced += 1;
    }
    Ok(replaced)
}

/// One region's read, ready for assembly. `text: None` = the read failed and
/// was already reported.
pub(crate) struct RegionRead {
    pub bbox: BBox,
    pub is_title: bool,
    pub rank: u32,
    pub text: Option<String>,
}

/// Everything that decides what lands on the page: reject empty and runaway
/// answers, build the positioned chunks, and apply the degradation gate.
/// `None` = keep the deterministic text.
///
/// Split from the render/detect/infer shell so it can be unit-tested — this is
/// the part that determines the output, and the part a backend swap touches.
pub(crate) fn build_replacement(
    page: &Page,
    reads: &[RegionRead],
    source: &str,
) -> Option<Vec<TextChunk>> {
    let new_chunks: Vec<TextChunk> = reads
        .iter()
        .filter_map(|r| {
            let text = r.text.as_deref()?.trim();
            // Out-of-domain repetition loop — hallucinated volume would
            // otherwise sail through the char-count gate below.
            if text.is_empty() || crate::unirec::looks_degenerate(text) {
                return None;
            }
            Some(TextChunk {
                text: text.to_string(),
                bbox: r.bbox,
                font_size: if r.is_title { 16.0 } else { 10.0 },
                font: None,
                page: page.number,
                confidence: 0.85,
                bold: false,
                hidden: false,
                source: Some(source.to_string()),
                group: Some(r.rank),
                tag: None,
            })
        })
        .collect();

    // Degradation gate: only swap when transcription holds its own against
    // what the deterministic parse already extracted.
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
        return None;
    }
    Some(new_chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(y: f32) -> BBox {
        BBox {
            x0: 10.0,
            y0: y,
            x1: 200.0,
            y1: y + 20.0,
        }
    }

    /// A deterministic page carrying `chars` characters of visible text — the
    /// baseline the degradation gate measures against.
    fn page_with(chars: usize) -> Page {
        Page {
            number: 7,
            width: 612.0,
            height: 792.0,
            elements: vec![Element::Text(TextChunk {
                text: "x".repeat(chars),
                bbox: bbox(100.0),
                font_size: 10.0,
                font: None,
                page: 7,
                confidence: 1.0,
                bold: false,
                hidden: false,
                source: None,
                group: None,
                tag: None,
            })],
        }
    }

    /// `chars` characters of non-repeating text. A naive `"y".repeat(n)` would
    /// trip `looks_degenerate` and make a gate test pass for the wrong reason.
    fn prose(chars: usize) -> String {
        let mut s = String::new();
        let mut i = 0usize;
        while s.chars().count() < chars {
            s.push_str(&format!("w{i} "));
            i += 1;
        }
        s.chars().take(chars).collect()
    }

    fn read(text: Option<&str>, is_title: bool, rank: u32) -> RegionRead {
        RegionRead {
            bbox: bbox(rank as f32 * 30.0),
            is_title,
            rank,
            text: text.map(str::to_string),
        }
    }

    /// T1b — the fields a backend swap touches: `source` comes from the
    /// reader's tag, titles get heading-sized font so downstream
    /// classification fires, `group` carries the region's reading rank, and
    /// confidence stays below 1.0 (model-produced, per the IR contract).
    #[test]
    fn chunks_carry_source_rank_and_reduced_confidence() {
        let page = page_with(10);
        let reads = vec![
            read(Some("A Heading"), true, 0),
            read(Some("body text here"), false, 1),
        ];
        let out = build_replacement(&page, &reads, "transcribe:vlm:OvisOCR2").expect("gate passes");

        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|c| c.source.as_deref() == Some("transcribe:vlm:OvisOCR2")));
        assert!(out.iter().all(|c| c.confidence < 1.0));
        assert!(out.iter().all(|c| c.page == 7));
        assert_eq!(out[0].font_size, 16.0, "title region reads as a heading");
        assert_eq!(out[1].font_size, 10.0);
        assert_eq!((out[0].group, out[1].group), (Some(0), Some(1)));
        // Geometry is the region's own, not synthesized from the answer.
        assert_eq!(
            (
                out[0].bbox.x0,
                out[0].bbox.y0,
                out[0].bbox.x1,
                out[0].bbox.y1
            ),
            (
                reads[0].bbox.x0,
                reads[0].bbox.y0,
                reads[0].bbox.x1,
                reads[0].bbox.y1
            )
        );
    }

    /// Failed reads (`text: None`) and runaway repetition are dropped rather
    /// than emitted — and, crucially, do NOT count toward the char gate.
    #[test]
    fn failed_and_degenerate_regions_are_dropped() {
        let page = page_with(10);
        let runaway = "abcdefg ".repeat(12);
        let reads = vec![
            read(None, false, 0),
            read(Some(&runaway), false, 1),
            read(Some("  real content that is long enough  "), false, 2),
        ];
        let out = build_replacement(&page, &reads, "transcribe:x").expect("gate passes");
        assert_eq!(out.len(), 1, "only the genuine region survives");
        assert_eq!(out[0].text, "real content that is long enough", "trimmed");
    }

    /// T4b — the degradation gate, which is transcribe-only (the table path
    /// has no char-ratio rule). Recovering materially less text than the
    /// deterministic parse already had keeps the original page.
    #[test]
    fn gate_keeps_deterministic_page_when_transcription_shrinks_it() {
        let page = page_with(100);
        // 40 chars vs 100 → below MIN_CHAR_RATIO (0.5).
        let reads = vec![read(Some(&prose(40)), false, 0)];
        assert!(build_replacement(&page, &reads, "transcribe:x").is_none());

        // 60 chars vs 100 → above the ratio, swap goes through.
        let reads = vec![read(Some(&prose(60)), false, 0)];
        assert!(build_replacement(&page, &reads, "transcribe:x").is_some());
    }

    /// An empty result never replaces a page, even when the deterministic
    /// parse found nothing either (no text at all is not an improvement).
    #[test]
    fn empty_result_never_replaces() {
        assert!(build_replacement(&page_with(0), &[], "transcribe:x").is_none());
        let reads = vec![read(Some("   "), false, 0)];
        assert!(build_replacement(&page_with(0), &reads, "transcribe:x").is_none());
    }
}
