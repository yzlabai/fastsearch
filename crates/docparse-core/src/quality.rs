//! Quality scoring — a format-agnostic read on how well a document parsed.
//!
//! This is the *contract* half of roadmap module 7: compute cheap, explainable
//! signals (text coverage, garbled-character ratio) that later (M7) drive
//! per-page routing to pluggable OCR/LLM fallback. Here we only *produce the
//! score*; nothing acts on it yet.

use crate::ir::{Document, Element};
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

    for page in &doc.pages {
        let mut page_has_text = false;
        for el in &page.elements {
            if let Element::Text(t) = el {
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

    QualityReport {
        pages,
        text_pages,
        coverage,
        total_chars,
        garbled_chars,
        garbled_ratio,
        flags,
    }
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
                bbox: BBox { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 },
                font_size: 10.0,
                font: None,
                page: number,
                confidence: 1.0,
            })]
        };
        Page { number, width: 100.0, height: 100.0, elements }
    }

    fn doc(pages: Vec<Page>) -> Document {
        Document { source: "t".into(), provenance: None, pages }
    }

    #[test]
    fn scanned_no_text_flagged() {
        let r = analyze(&doc(vec![page_with(1, ""), page_with(2, "")]));
        assert_eq!(r.coverage, 0.0);
        assert!(r.flags.contains(&QualityFlag::ScannedNoText));
    }

    #[test]
    fn clean_digital_doc_has_no_flags() {
        let r = analyze(&doc(vec![page_with(1, "Hello world"), page_with(2, "More text")]));
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
