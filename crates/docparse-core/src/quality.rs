//! Quality scoring — a format-agnostic read on how well a document parsed.
//!
//! This is the *contract* half of roadmap module 7: compute cheap, explainable
//! signals (text coverage, garbled-character ratio) that later (M7) drive
//! per-page routing to pluggable OCR/LLM fallback. Here we only *produce the
//! score*; nothing acts on it yet.

use crate::ir::{Document, Element, Page};
use serde::{Deserialize, Serialize};

/// A reason the deterministic parse may be insufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityFlag {
    /// Pages exist but no text was extracted — almost certainly a scan needing OCR.
    ScannedNoText,
    /// Some pages have no text while others do — a mixed/hybrid document.
    PartialTextCoverage,
    /// A high fraction of decoded characters look garbled (control/replacement).
    HighGarble,
    /// Human-invisible text was found and excluded from rendered outputs
    /// (prompt-injection vector; N5a). Count in `hidden_chunks`; the flagged
    /// chunks remain in the IR JSON for audit.
    HiddenTextPresent,
    /// The page has a real text layer AND a large raster with pixel payload
    /// (e.g. an inserted scan/stamp) — region-level OCR can recover the
    /// raster's text without touching the digital text (G4).
    MixedTextAndScan,
}

/// A computed quality read on a [`Document`]. Serializable for CLI/observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub pages: usize,
    /// Pages with at least one text chunk.
    pub text_pages: usize,
    /// `text_pages / pages` in [0,1].
    pub coverage: f32,
    pub total_chars: usize,
    /// Control (non-whitespace) or U+FFFD characters.
    pub garbled_chars: usize,
    /// `garbled_chars / total_chars` in [0,1].
    pub garbled_ratio: f32,
    /// Text chunks classified hidden (invisible render mode / off-page / tiny
    /// font) and excluded from rendered outputs.
    #[serde(default)]
    pub hidden_chunks: usize,
    pub flags: Vec<QualityFlag>,
}

impl QualityReport {
    /// Pretty-JSON for CLI/observability output.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Whether a character counts as "garbled" for the heuristic: replacement char,
/// or a control char that is not ordinary whitespace.
fn is_garbled(c: char) -> bool {
    c == '\u{FFFD}' || (c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}

/// Compute a [`QualityReport`] for a parsed document.
pub fn analyze(doc: &Document) -> QualityReport {
    let pages = doc.pages.len();
    let mut text_pages = 0;
    let mut total_chars = 0usize;
    let mut garbled_chars = 0usize;
    let mut hidden_chunks = 0usize;

    for page in &doc.pages {
        let mut page_has_text = false;
        for el in &page.elements {
            if let Element::Text(t) = el {
                // Hidden text is excluded from every visible signal — it
                // shouldn't count as page coverage — but is tallied for audit.
                if t.hidden {
                    hidden_chunks += 1;
                    continue;
                }
                for c in t.text.chars() {
                    total_chars += 1;
                    if is_garbled(c) {
                        garbled_chars += 1;
                    }
                    if !c.is_whitespace() {
                        page_has_text = true;
                    }
                }
            }
        }
        if page_has_text {
            text_pages += 1;
        }
    }

    let coverage = if pages == 0 {
        0.0
    } else {
        text_pages as f32 / pages as f32
    };
    let garbled_ratio = if total_chars == 0 {
        0.0
    } else {
        garbled_chars as f32 / total_chars as f32
    };

    let mut flags = Vec::new();
    if pages > 0 && text_pages == 0 {
        flags.push(QualityFlag::ScannedNoText);
    } else if text_pages < pages {
        flags.push(QualityFlag::PartialTextCoverage);
    }
    if garbled_ratio > 0.1 {
        flags.push(QualityFlag::HighGarble);
    }
    if hidden_chunks > 0 {
        flags.push(QualityFlag::HiddenTextPresent);
    }

    QualityReport {
        pages,
        text_pages,
        coverage,
        total_chars,
        garbled_chars,
        garbled_ratio,
        hidden_chunks,
        flags,
    }
}

/// Per-page quality, used by routing (M7) to decide whether a page should be
/// escalated to a pluggable enhancer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageAssessment {
    pub page: usize,
    pub chars: usize,
    pub garbled_ratio: f32,
    pub flags: Vec<QualityFlag>,
    /// True when the deterministic parse looks insufficient (no text, or very
    /// garbled) — a candidate for OCR/LLM fallback.
    pub needs_enhancement: bool,
}

/// Assess one page: no text ⇒ likely scan; high garble ⇒ bad decode.
pub fn assess_page(page: &Page) -> PageAssessment {
    let mut chars = 0usize;
    let mut garbled = 0usize;
    let mut has_text = false;
    for el in &page.elements {
        if let Element::Text(t) = el {
            for c in t.text.chars() {
                chars += 1;
                if is_garbled(c) {
                    garbled += 1;
                }
                if !c.is_whitespace() {
                    has_text = true;
                }
            }
        }
    }
    let garbled_ratio = if chars == 0 {
        0.0
    } else {
        garbled as f32 / chars as f32
    };
    let mut flags = Vec::new();
    if !has_text {
        flags.push(QualityFlag::ScannedNoText);
    }
    if garbled_ratio > 0.1 {
        flags.push(QualityFlag::HighGarble);
    }
    // Mixed page: digital text + a raster that carries pixels (scan-shaped).
    if has_text
        && page
            .elements
            .iter()
            .any(|e| matches!(e, Element::Image(i) if !i.data.is_empty()))
    {
        flags.push(QualityFlag::MixedTextAndScan);
    }
    PageAssessment {
        page: page.number,
        chars,
        garbled_ratio,
        needs_enhancement: !flags.is_empty(),
        flags,
    }
}

/// Assess every page of a document.
pub fn assess_pages(doc: &Document) -> Vec<PageAssessment> {
    doc.pages.iter().map(assess_page).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BBox, Element, Page, TextChunk};

    fn page_with(number: usize, text: &str) -> Page {
        let elements = if text.is_empty() {
            vec![]
        } else {
            vec![Element::Text(TextChunk {
                text: text.to_string(),
                bbox: BBox {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0,
                },
                font_size: 10.0,
                font: None,
                page: number,
                confidence: 1.0,
                bold: false,
                hidden: false,
                source: None,
                group: None,
                tag: None,
            })]
        };
        Page {
            number,
            width: 100.0,
            height: 100.0,
            elements,
        }
    }

    fn doc(pages: Vec<Page>) -> Document {
        Document {
            source: "t".into(),
            provenance: None,
            pages,
        }
    }

    #[test]
    fn scanned_no_text_flagged() {
        let r = analyze(&doc(vec![page_with(1, ""), page_with(2, "")]));
        assert_eq!(r.coverage, 0.0);
        assert!(r.flags.contains(&QualityFlag::ScannedNoText));
    }

    #[test]
    fn clean_digital_doc_has_no_flags() {
        let r = analyze(&doc(vec![
            page_with(1, "Hello world"),
            page_with(2, "More text"),
        ]));
        assert_eq!(r.coverage, 1.0);
        assert_eq!(r.garbled_ratio, 0.0);
        assert!(r.flags.is_empty());
    }

    #[test]
    fn partial_coverage_flagged() {
        let r = analyze(&doc(vec![page_with(1, "text"), page_with(2, "")]));
        assert!(r.flags.contains(&QualityFlag::PartialTextCoverage));
        assert_eq!(r.text_pages, 1);
    }

    #[test]
    fn garbled_control_chars_counted() {
        let r = analyze(&doc(vec![page_with(1, "ok\u{0}\u{1}\u{FFFD}")]));
        assert_eq!(r.garbled_chars, 3);
        assert!(r.garbled_ratio > 0.1);
        assert!(r.flags.contains(&QualityFlag::HighGarble));
    }
}

/// What a page fundamentally is — the complexity-profile signal (module 9,
/// N5c) that routing and operators consume. Derived purely from the IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    /// Text layer present, no page-covering raster — the fast path.
    Digital,
    /// No text layer, a page-covering raster — OCR territory.
    Scanned,
    /// Both a text layer and a page-covering raster (stamped/hybrid pages).
    Mixed,
    /// Neither text nor images (blank or wholly unsupported content).
    Empty,
}

/// Per-page complexity profile. Cheap to compute, explainable, serializable —
/// feeds routing decisions and the agent-facing quality envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageProfile {
    pub page: usize,
    pub kind: PageKind,
    /// Visible (non-hidden) text characters.
    pub text_chars: usize,
    pub image_count: usize,
    /// Largest image's share of the page area, in [0,1].
    pub image_coverage: f32,
    pub tables: usize,
    /// Text chunks produced by an enhancer (`source` set) — 0 on a pure
    /// deterministic parse.
    pub enhanced_chunks: usize,
}

/// A page-covering raster at or above this share of the page marks the page
/// scan-shaped (mirrors the interpreter's pixel-attachment gate).
const PAGE_COVERING: f32 = 0.5;

/// Profile one page from its IR elements.
pub fn profile_page(page: &Page) -> PageProfile {
    let page_area = (page.width * page.height).max(1.0);
    let mut text_chars = 0usize;
    let mut image_count = 0usize;
    let mut image_coverage = 0.0f32;
    let mut tables = 0usize;
    let mut enhanced_chunks = 0usize;
    for el in &page.elements {
        match el {
            Element::Text(t) => {
                if !t.hidden {
                    text_chars += t.text.chars().filter(|c| !c.is_whitespace()).count();
                }
                if t.source.is_some() {
                    enhanced_chunks += 1;
                }
            }
            Element::Image(i) => {
                image_count += 1;
                let a = ((i.bbox.x1 - i.bbox.x0) * (i.bbox.y1 - i.bbox.y0)) / page_area;
                image_coverage = image_coverage.max(a.clamp(0.0, 1.0));
            }
            Element::Table(_) => tables += 1,
        }
    }
    let covered = image_coverage >= PAGE_COVERING;
    let kind = match (text_chars > 0, covered, image_count) {
        (true, true, _) => PageKind::Mixed,
        (true, false, _) => PageKind::Digital,
        (false, true, _) => PageKind::Scanned,
        (false, false, 0) => PageKind::Empty,
        // Images only, none page-covering (decorative/figures, no text).
        (false, false, _) => PageKind::Scanned,
    };
    PageProfile {
        page: page.number,
        kind,
        text_chars,
        image_count,
        image_coverage,
        tables,
        enhanced_chunks,
    }
}

/// Profile every page of a document.
pub fn profile(doc: &Document) -> Vec<PageProfile> {
    doc.pages.iter().map(profile_page).collect()
}

/// Pretty-JSON profile array (CLI/observability).
pub fn profile_json(profiles: &[PageProfile]) -> String {
    serde_json::to_string_pretty(profiles).unwrap_or_default()
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::ir::{BBox, Element, ImageChunk, ImageKind, Page, TextChunk};

    fn text(t: &str, source: Option<&str>) -> Element {
        Element::Text(TextChunk {
            text: t.into(),
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: source.map(Into::into),
            group: None,
            tag: None,
        })
    }

    fn image(cover: f32) -> Element {
        Element::Image(ImageChunk {
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 100.0 * cover,
                y1: 100.0,
            },
            page: 1,
            width_px: 100,
            height_px: 100,
            kind: ImageKind::None,
            data: Vec::new(),
        })
    }

    fn page(elements: Vec<Element>) -> Page {
        Page {
            number: 1,
            width: 100.0,
            height: 100.0,
            elements,
        }
    }

    #[test]
    fn classifies_the_four_kinds() {
        assert_eq!(
            profile_page(&page(vec![text("hi", None)])).kind,
            PageKind::Digital
        );
        assert_eq!(
            profile_page(&page(vec![image(0.9)])).kind,
            PageKind::Scanned
        );
        assert_eq!(
            profile_page(&page(vec![text("hi", None), image(0.9)])).kind,
            PageKind::Mixed
        );
        assert_eq!(profile_page(&page(vec![])).kind, PageKind::Empty);
        // Small figure + text stays digital; figure-only page counts scanned.
        assert_eq!(
            profile_page(&page(vec![text("hi", None), image(0.2)])).kind,
            PageKind::Digital
        );
    }

    #[test]
    fn counts_signals() {
        let p = profile_page(&page(vec![
            text("abc def", None),
            text("ocr line", Some("ocr:ppocr-v4")),
            image(0.8),
        ]));
        assert_eq!(p.text_chars, 6 + 7);
        assert_eq!(p.image_count, 1);
        assert!((p.image_coverage - 0.8).abs() < 1e-5);
        assert_eq!(p.enhanced_chunks, 1);
        assert_eq!(p.kind, PageKind::Mixed);
    }
}
