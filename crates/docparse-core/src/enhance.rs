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

use crate::ir::{Document, Page};
use crate::quality::{self, PageAssessment, QualityFlag};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Per-in-flight-page scan-buffer budget. Enhancement is CPU-bound (det conv
/// net + per-line rec) and pages are independent, so it parallelizes like the
/// deterministic parse — but each in-flight scan buffer is ~100MB, so *memory*,
/// not cores, is the binding constraint on how many pages may be in flight.
const PER_PAGE_BUDGET_BYTES: u64 = 100 * 1024 * 1024;

/// Fallback parallelism when physical memory can't be queried — the historical
/// fixed cap that sat past the efficiency knee on an 18-core box (8→18 threads
/// only lifts 5.5×→10×) while keeping peak buffer memory bounded.
const FALLBACK_PARALLELISM: usize = 8;

/// Choose the page-parallel enhancement width from cores and physical RAM.
///
/// The old fixed cap of 8 left ~2× throughput on the table on big-RAM/high-core
/// boxes (where 18 threads is safe) yet could still over-commit memory on a
/// low-RAM/high-core box. Instead we let *memory* decide: budget half of
/// physical RAM to scan buffers (leaving the rest for models + the OS) and fit
/// as many ~100MB pages as that allows, never exceeding the core count.
///
/// Pure so it can be unit-tested without a real machine. `total_ram` is `None`
/// when the platform query failed → fall back to the historical 8. An explicit
/// `DOCPARSE_OCR_PARALLELISM` override (see [`ocr_pool`]) bypasses this entirely.
fn desired_parallelism(cores: usize, total_ram: Option<u64>) -> usize {
    let cores = cores.max(1);
    match total_ram {
        Some(ram) => {
            let budget = ram / 2;
            let by_mem = (budget / PER_PAGE_BUDGET_BYTES).max(1) as usize;
            by_mem.min(cores)
        }
        None => cores.min(FALLBACK_PARALLELISM),
    }
}

/// Total physical RAM in bytes, or `None` if the platform can't report it.
/// Uses `libc` (already in-tree, same supply-chain surface as the `--stats`
/// `getrusage` path) rather than a heavier system-info crate.
fn total_physical_ram() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut mem: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // hw.memsize is the physical RAM size in bytes.
        let name = c"hw.memsize";
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut mem as *mut u64 as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (rc == 0 && mem > 0).then_some(mem)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
        (pages > 0 && page_size > 0).then(|| pages as u64 * page_size as u64)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Read the `DOCPARSE_OCR_PARALLELISM` override, if set to a positive integer.
fn parallelism_override() -> Option<usize> {
    std::env::var("DOCPARSE_OCR_PARALLELISM")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
}

/// Shared, bounded worker pool for page-parallel enhancement — built once and
/// reused so concurrent callers (e.g. the REST/MCP server handling parallel
/// requests, each via `spawn_blocking`) share one set of workers rather than
/// each spawning its own pool (thread blow-up + per-call build churn). The
/// width adapts to physical memory ([`desired_parallelism`]), overridable via
/// `DOCPARSE_OCR_PARALLELISM`. Returns `None` if the OS refuses the threads, in
/// which case `apply` degrades to serial instead of panicking.
fn ocr_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|c| c.get())
            .unwrap_or(1);
        let threads = parallelism_override()
            .map(|n| n.min(cores))
            .unwrap_or_else(|| desired_parallelism(cores, total_physical_ram()));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .ok()
    })
    .as_ref()
}

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
            // A scan region pasted into a digital page — OCR territory.
            QualityFlag::MixedTextAndScan => self.handles_scanned,
        }
    }
}

/// An external enhancer (OCR/LLM/VLM behind a uniform boundary). Implementors
/// live outside the deterministic core and are injected by the caller.
pub trait Enhancer: Send + Sync {
    fn capability(&self) -> Capability;
    /// Process one flagged page, returning the replacement page, or `None`
    /// to decline. Implementors should set `confidence < 1.0` on produced
    /// text. Returning a whole `Page` (not just elements) lets an enhancer
    /// correct page geometry as well — e.g. orientation-normalizing a rotated
    /// scan (H2) swaps width/height; `number` must be kept.
    fn enhance_page(&self, page: &Page) -> Option<Page>;
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

/// One page's routing: assess, run the first capable enhancer, merge its output.
/// Returns the (possibly replaced) page plus the route record (`None` when the
/// page didn't need enhancement). Pure and independent — safe to run in parallel.
fn process_page(page: &Page, enhancers: &[&dyn Enhancer]) -> (Page, Option<PageRoute>) {
    let assessment = quality::assess_page(page);
    if !assessment.needs_enhancement {
        return (page.clone(), None);
    }
    let mut route = PageRoute {
        page: page.number,
        flags: assessment.flags.clone(),
        enhancer: None,
        applied: false,
    };
    let mut replaced = None;
    for e in enhancers {
        let cap = e.capability();
        if !assessment.flags.iter().any(|&f| cap.covers(f)) {
            continue;
        }
        route.enhancer = Some(cap.name.clone());
        if let Some(enhanced) = e.enhance_page(page) {
            replaced = Some(enhanced);
            route.applied = true;
            break;
        }
    }
    (replaced.unwrap_or_else(|| page.clone()), Some(route))
}

/// Apply routing: run the first capable enhancer on each flagged page and merge
/// its output. The deterministic pages pass through untouched. Returns the new
/// document and the per-page routing report.
pub fn apply(doc: &Document, enhancers: &[&dyn Enhancer]) -> (Document, Vec<PageRoute>) {
    apply_with(doc, enhancers, None)
}

/// Like [`apply`], but invokes `on_page` once per page as it finishes (in
/// whatever order the parallel workers complete) — the hook the CLI feeds to a
/// progress bar. `Sync` because the page loop runs on the shared rayon pool;
/// the callback must do its own synchronization (a thread-safe progress bar
/// does). Pass `None` for the plain behavior. Output stays byte-identical to
/// [`apply`] regardless of the callback.
pub fn apply_with(
    doc: &Document,
    enhancers: &[&dyn Enhancer],
    on_page: Option<&(dyn Fn() + Sync)>,
) -> (Document, Vec<PageRoute>) {
    // Per-page work: assess, route to the first capable enhancer, and merge its
    // output. Pure — reads the page, returns a (possibly replaced) page plus the
    // route record (`None` for pages that didn't need enhancement, matching the
    // old loop's `continue`-before-push). No cross-page shared state, so pages
    // run independently in parallel below. The progress callback fires on every
    // page (enhanced or passed-through) so the bar tracks pages processed.
    let process = |page: &Page| -> (Page, Option<PageRoute>) {
        let result = process_page(page, enhancers);
        if let Some(cb) = on_page {
            cb();
        }
        result
    };

    // Parallelize across pages (CPU-bound, independent) through the shared
    // bounded pool so peak scan-buffer memory stays capped and concurrent
    // callers (REST/MCP server) share workers instead of each spawning a pool.
    // An indexed `par_iter().collect()` preserves page order, keeping output
    // byte-identical to the serial path. Single-page docs, or a pool the OS
    // refused to build, run serially — never panic on a resource shortage.
    let results: Vec<(Page, Option<PageRoute>)> = match ocr_pool() {
        Some(pool) if doc.pages.len() > 1 => {
            pool.install(|| doc.pages.par_iter().map(&process).collect())
        }
        _ => doc.pages.iter().map(&process).collect(),
    };

    // Split results into pages + report. Each page was cloned at most once (in
    // `process`); reconstruct the doc cloning only the light scalar fields so
    // ~100MB scan buffers aren't copied a second time via `doc.clone()`.
    let mut pages = Vec::with_capacity(results.len());
    let mut report = Vec::new();
    for (page, route) in results {
        pages.push(page);
        if let Some(route) = route {
            report.push(route);
        }
    }
    let out = Document {
        source: doc.source.clone(),
        provenance: doc.provenance.clone(),
        pages,
    };
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

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn parallelism_scales_with_ram_but_never_exceeds_cores() {
        // Big-RAM/high-core box: fit far more than 18 pages, so cores bind.
        assert_eq!(desired_parallelism(18, Some(32 * GB)), 18);
        // Modest 4-core/8GB box: 4GB budget / 100MB ≈ 40 pages → cores bind.
        assert_eq!(desired_parallelism(4, Some(8 * GB)), 4);
    }

    #[test]
    fn parallelism_is_memory_bound_when_ram_is_tight() {
        // 16 cores but only ~512MB: budget 256MB / 100MB = 2 pages in flight.
        assert_eq!(desired_parallelism(16, Some(512 * 1024 * 1024)), 2);
        // Always at least one page, even on a sliver of RAM.
        assert_eq!(desired_parallelism(8, Some(64 * 1024 * 1024)), 1);
    }

    #[test]
    fn parallelism_falls_back_to_fixed_cap_without_ram_info() {
        // Unknown RAM → historical fixed cap of 8, still bounded by cores.
        assert_eq!(desired_parallelism(18, None), FALLBACK_PARALLELISM);
        assert_eq!(desired_parallelism(2, None), 2);
        // Degenerate zero-core report still yields a usable width.
        assert_eq!(desired_parallelism(0, Some(32 * GB)), 1);
    }

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
        fn enhance_page(&self, page: &Page) -> Option<Page> {
            Some(Page {
                number: page.number,
                width: page.width,
                height: page.height,
                elements: vec![Element::Text(TextChunk {
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
                    group: None,
                    tag: None,
                })],
            })
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
                group: None,
                tag: None,
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

    #[test]
    fn parallel_apply_preserves_order_and_is_deterministic() {
        // More pages than MAX_PAGE_PARALLELISM so the page-parallel path runs in
        // several waves; alternate digital/scanned so routing is non-trivial.
        let pages: Vec<Page> = (1..=20)
            .map(|n| {
                if n % 2 == 0 {
                    page(n, None)
                } else {
                    page(n, Some("digital"))
                }
            })
            .collect();
        let d = doc(pages);
        let ocr = StubOcr;
        let (out, report) = apply(&d, &[&ocr]);

        // Page order preserved; each page handled per its kind (parallel collect
        // must be byte-identical to the serial loop — the determinism contract).
        assert_eq!(out.pages.len(), 20);
        for (i, p) in out.pages.iter().enumerate() {
            let n = i + 1;
            assert_eq!(p.number, n, "page order preserved");
            match &p.elements[0] {
                Element::Text(t) if n % 2 == 0 => {
                    assert!(t.text.starts_with("[ocr]"), "scanned page {n} enhanced")
                }
                Element::Text(t) => assert_eq!(t.text, "digital", "digital page {n} untouched"),
                _ => panic!("expected text on page {n}"),
            }
        }

        // Report holds only the scanned pages, in ascending page order.
        let routed: Vec<usize> = report.iter().map(|r| r.page).collect();
        let expected: Vec<usize> = (1..=20).filter(|n| n % 2 == 0).collect();
        assert_eq!(routed, expected);
        assert!(report.iter().all(|r| r.applied));

        // Determinism: identical output across runs regardless of thread schedule.
        let texts = |dd: &Document| -> Vec<String> {
            dd.pages
                .iter()
                .map(|p| match &p.elements[0] {
                    Element::Text(t) => t.text.clone(),
                    _ => unreachable!(),
                })
                .collect()
        };
        let (out2, report2) = apply(&d, &[&ocr]);
        assert_eq!(texts(&out), texts(&out2));
        assert_eq!(routed, report2.iter().map(|r| r.page).collect::<Vec<_>>());
    }

    #[test]
    fn apply_with_fires_callback_once_per_page_and_matches_apply() {
        // Mixed digital/scanned, more pages than the pool so the callback fires
        // from several workers concurrently.
        let pages: Vec<Page> = (1..=12)
            .map(|n| {
                if n % 3 == 0 {
                    page(n, None)
                } else {
                    page(n, Some("digital"))
                }
            })
            .collect();
        let d = doc(pages);
        let ocr = StubOcr;

        // Callback fires for EVERY page (enhanced or passed-through) so a progress
        // bar reaches the total. Atomic because the page loop runs in parallel.
        let count = std::sync::atomic::AtomicUsize::new(0);
        let on_page = || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        };
        let (with_out, with_report) = apply_with(&d, &[&ocr], Some(&on_page));
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            12,
            "callback fires exactly once per page"
        );

        // The callback must not change the result: byte-for-byte equal to apply().
        let (plain_out, plain_report) = apply(&d, &[&ocr]);
        let texts = |dd: &Document| -> Vec<String> {
            dd.pages
                .iter()
                .map(|p| match &p.elements[0] {
                    Element::Text(t) => t.text.clone(),
                    _ => unreachable!(),
                })
                .collect()
        };
        assert_eq!(texts(&with_out), texts(&plain_out));
        assert_eq!(
            with_report.iter().map(|r| r.page).collect::<Vec<_>>(),
            plain_report.iter().map(|r| r.page).collect::<Vec<_>>(),
        );
    }
}
