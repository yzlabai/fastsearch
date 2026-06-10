//! Pluggable enhancement boundary + quality-driven routing (roadmap modules
//! 7–8). This is the cost thesis made concrete: the deterministic parse runs
//! independently and produces a complete result; only pages the quality score
//! flags as hard are escalated, *per page*, to an optional external enhancer
//! (OCR/LLM/VLM). Most pages never touch a model.
//!
//! The main parse path NEVER calls this — enhancement is opt-in. An [`Enhancer`]
//! advertises a versioned [`Capability`]; routing matches a page's quality flags
//! to a capable enhancer and merges its output back into the same IR (with
//! lowered confidence so downstream can tell deterministic from model output).

use crate::ir::{Document, Element, Page};
use crate::quality::{self, PageAssessment, QualityFlag};
use serde::{Deserialize, Serialize};

/// What an enhancer can do, versioned for reproducibility/observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    pub version: String,
    /// Can recover text from a page with no text layer (scanned image).
    pub handles_scanned: bool,
    /// Can re-decode badly garbled text.
    pub handles_garbled: bool,
}

impl Capability {
    /// Whether this capability addresses the given quality flag.
    fn covers(&self, flag: QualityFlag) -> bool {
        match flag {
            QualityFlag::ScannedNoText => self.handles_scanned,
            QualityFlag::HighGarble => self.handles_garbled,
            QualityFlag::PartialTextCoverage => self.handles_scanned,
            // Hidden text is already filtered deterministically (N5a); it is
            // an audit signal, not a deficiency a model could repair.
            QualityFlag::HiddenTextPresent => false,
        }
    }
}

/// An external enhancer (OCR/LLM/VLM behind a uniform boundary). Implementors
/// live outside the deterministic core and are injected by the caller.
pub trait Enhancer: Send + Sync {
    fn capability(&self) -> Capability;
    /// Process one flagged page, returning replacement elements, or `None` to
    /// decline. Implementors should set `confidence < 1.0` on produced text.
    fn enhance_page(&self, page: &Page) -> Option<Vec<Element>>;
}

/// The routing decision for one page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRoute {
    pub page: usize,
    pub flags: Vec<QualityFlag>,
    /// Name of the enhancer chosen to handle this page, if any was capable.
    pub enhancer: Option<String>,
    /// Whether enhancement actually replaced the page (the enhancer ran and
    /// returned content).
    pub applied: bool,
}

/// Plan routing without running anything: for each page that needs enhancement,
/// which (if any) available enhancer would handle it. Pure/cheap — use it to
/// report the hard-page rate and pick fallback before paying for a model.
pub fn plan(doc: &Document, enhancers: &[&dyn Enhancer]) -> Vec<PageRoute> {
    let caps: Vec<Capability> = enhancers.iter().map(|e| e.capability()).collect();
    quality::assess_pages(doc)
        .into_iter()
        .filter(|a| a.needs_enhancement)
        .map(|a: PageAssessment| {
            let enhancer = caps
                .iter()
                .find(|c| a.flags.iter().any(|&f| c.covers(f)))
                .map(|c| c.name.clone());
            PageRoute {
                page: a.page,
                flags: a.flags,
                enhancer,
                applied: false,
            }
        })
        .collect()
}

/// Apply routing: run the first capable enhancer on each flagged page and merge
/// its output. The deterministic pages pass through untouched. Returns the new
/// document and the per-page routing report.
pub fn apply(doc: &Document, enhancers: &[&dyn Enhancer]) -> (Document, Vec<PageRoute>) {
    let mut out = doc.clone();
    let mut report = Vec::new();

    for page in &mut out.pages {
        let assessment = quality::assess_page(page);
        if !assessment.needs_enhancement {
            continue;
        }
        let mut route = PageRoute {
            page: page.number,
            flags: assessment.flags.clone(),
            enhancer: None,
            applied: false,
        };
        for e in enhancers {
            let cap = e.capability();
            if !assessment.flags.iter().any(|&f| cap.covers(f)) {
                continue;
            }
            route.enhancer = Some(cap.name.clone());
            if let Some(elements) = e.enhance_page(page) {
                page.elements = elements;
                route.applied = true;
                break;
            }
        }
        report.push(route);
    }
    (out, report)
}

/// Serialize a routing report as pretty JSON.
pub fn report_json(routes: &[PageRoute]) -> String {
    serde_json::to_string_pretty(routes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BBox, Element, Page, TextChunk};

    /// Stub OCR: "recovers" text from a no-text page with low confidence.
    struct StubOcr;
    impl Enhancer for StubOcr {
        fn capability(&self) -> Capability {
            Capability {
                name: "stub-ocr".into(),
                version: "0.0.1".into(),
                handles_scanned: true,
                handles_garbled: false,
            }
        }
        fn enhance_page(&self, page: &Page) -> Option<Vec<Element>> {
            Some(vec![Element::Text(TextChunk {
                text: "[ocr] recovered text".into(),
                bbox: BBox {
                    x0: 0.0,
                    y0: 0.0,
                    x1: page.width,
                    y1: 20.0,
                },
                font_size: 10.0,
                font: None,
                page: page.number,
                confidence: 0.5,
                bold: false,
                hidden: false,
                source: None,
            })])
        }
    }

    fn page(number: usize, text: Option<&str>) -> Page {
        let elements = match text {
            Some(t) => vec![Element::Text(TextChunk {
                text: t.into(),
                bbox: BBox {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 10.0,
                    y1: 10.0,
                },
                font_size: 10.0,
                font: None,
                page: number,
                confidence: 1.0,
                bold: false,
                hidden: false,
                source: None,
            })],
            None => vec![],
        };
        Page {
            number,
            width: 612.0,
            height: 792.0,
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
    fn only_hard_pages_are_routed() {
        // page 1 digital (text), page 2 scanned (no text).
        let d = doc(vec![page(1, Some("hello world")), page(2, None)]);
        let ocr = StubOcr;
        let routes = plan(&d, &[&ocr]);
        assert_eq!(routes.len(), 1, "only the scanned page is routed");
        assert_eq!(routes[0].page, 2);
        assert_eq!(routes[0].enhancer.as_deref(), Some("stub-ocr"));
    }

    #[test]
    fn apply_replaces_only_the_scanned_page() {
        let d = doc(vec![page(1, Some("hello world")), page(2, None)]);
        let ocr = StubOcr;
        let (out, report) = apply(&d, &[&ocr]);
        // Digital page untouched.
        assert_eq!(out.pages[0].elements.len(), 1);
        if let Element::Text(t) = &out.pages[0].elements[0] {
            assert_eq!(t.text, "hello world");
            assert_eq!(t.confidence, 1.0);
        } else {
            panic!("expected text");
        }
        // Scanned page enhanced, with low confidence.
        if let Element::Text(t) = &out.pages[1].elements[0] {
            assert!(t.text.starts_with("[ocr]"));
            assert_eq!(t.confidence, 0.5);
        } else {
            panic!("expected enhanced text");
        }
        assert_eq!(report.len(), 1);
        assert!(report[0].applied);
    }

    #[test]
    fn no_enhancers_means_no_changes() {
        let d = doc(vec![page(1, None)]);
        let (out, report) = apply(&d, &[]);
        assert!(
            out.pages[0].elements.is_empty(),
            "main flow independent of enhancers"
        );
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].enhancer, None);
        assert!(!report[0].applied);
    }
}
