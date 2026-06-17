//! Layout enhancer (plan G2): DocLayout-YOLO on `tract`, dictating macro
//! reading order on hard pages.
//!
//! Flow per routed page: render via `docparse-raster` (the only place pixels
//! exist) → detect layout regions (title/text/table/figure/… as decoded
//! YOLOv10 boxes, no NMS needed) → order regions with the core XY-cut (the
//! same deterministic geometry, now at region granularity) → tag each text
//! chunk with its region's order index (`TextChunk.group`). Downstream line
//! reconstruction orders groups first, then runs the usual geometry within
//! each — the model fixes the macro order, determinism keeps the micro order.

use anyhow::{Context, Result};
use docparse_core::ir::{BBox, Element, Page, Table, TextChunk};
use docparse_core::reading_order::reading_order;
use std::path::Path;
use tract_onnx::prelude::*;

type Runnable = std::sync::Arc<TypedRunnableModel>;

/// Detection canvas (DocLayout-YOLO contract).
const SIDE: usize = 1024;
/// PP-DocLayoutV2 input canvas (RT-DETR contract: exact resize, no letterbox).
const PPV2_SIDE: usize = 800;
/// Keep regions at or above this score (DocLayout-YOLO path).
const SCORE_MIN: f32 = 0.25;
/// PP-DocLayoutV2's own postprocess threshold (matches the official pipeline).
const PPV2_SCORE_MIN: f32 = 0.5;
/// Skip enhancement when fewer regions than this (nothing to reorder).
const MIN_REGIONS: usize = 2;
/// Require at least this fraction of text chunks to land in a region before
/// grouping takes effect: partial coverage would push the uncovered majority
/// behind the covered minority and scramble the page.
const MIN_COVERAGE: f32 = 0.7;
/// A render whose pixels are mostly dark is assumed broken (document pages
/// are predominantly light). Sampled, cheap, conservative.
const BROKEN_RENDER_DARK_MAX: f32 = 0.4;

/// Backend-agnostic semantic role of a layout region. Both DocLayout-YOLO
/// (DocStructBench, ~10 classes) and PP-DocLayoutV2 (25 classes) map their raw
/// class ids into this so downstream (table seeding, formula/transcribe
/// routing, heading tagging) need not know which model produced the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    Title,
    SubTitle,
    Abstract,
    Text,
    Table,
    Figure,
    Caption,
    Formula,
    InlineFormula,
    Header,
    Footer,
    Footnote,
    Reference,
    PageNumber,
    Other,
}

impl RegionKind {
    pub fn is_table(self) -> bool {
        self == RegionKind::Table
    }
    /// Block-level (display) formula — the formula-model's target.
    pub fn is_formula_block(self) -> bool {
        self == RegionKind::Formula
    }
    /// Body-text-like region (transcribe target).
    pub fn is_textual(self) -> bool {
        matches!(
            self,
            RegionKind::Text
                | RegionKind::Title
                | RegionKind::SubTitle
                | RegionKind::Abstract
                | RegionKind::Caption
                | RegionKind::Footnote
                | RegionKind::Reference
        )
    }
    pub fn is_title(self) -> bool {
        matches!(self, RegionKind::Title | RegionKind::SubTitle)
    }
    /// Structure role for tagging text chunks (cf. tagged-PDF "H1".."H6"/"P"),
    /// flowing the model's semantics into output. `None` = no heading role.
    fn tag_role(self) -> Option<&'static str> {
        match self {
            RegionKind::Title => Some("H1"),
            RegionKind::SubTitle => Some("H2"),
            _ => None,
        }
    }
}

/// DocLayout-YOLO DocStructBench class id → [`RegionKind`].
fn map_yolo(class: u8) -> RegionKind {
    match class {
        0 => RegionKind::Title,
        1 => RegionKind::Text,  // plain text
        2 => RegionKind::Other, // abandon (header/footer/marginalia)
        3 => RegionKind::Figure,
        4 => RegionKind::Caption, // figure_caption
        5 => RegionKind::Table,
        6 => RegionKind::Caption,  // table_caption
        7 => RegionKind::Footnote, // table_footnote
        8 => RegionKind::Formula,  // isolate_formula
        9 => RegionKind::Caption,  // formula_caption
        _ => RegionKind::Other,
    }
}

/// PP-DocLayoutV2 class id (25-class taxonomy) → [`RegionKind`].
fn map_ppv2(class: u8) -> RegionKind {
    match class {
        0 => RegionKind::Abstract,
        1 => RegionKind::Text,       // algorithm
        2 => RegionKind::Text,       // aside_text
        3 => RegionKind::Figure,     // chart
        4 => RegionKind::Text,       // content (toc-ish)
        5 => RegionKind::Formula,    // display_formula
        6 => RegionKind::Title,      // doc_title
        7 => RegionKind::Caption,    // figure_title
        8 | 9 => RegionKind::Footer, // footer / footer_image
        10 => RegionKind::Footnote,
        11 => RegionKind::Other,       // formula_number
        12 | 13 => RegionKind::Header, // header / header_image
        14 => RegionKind::Figure,      // image
        15 => RegionKind::InlineFormula,
        16 => RegionKind::PageNumber,
        17 => RegionKind::SubTitle, // paragraph_title
        18 | 19 => RegionKind::Reference,
        20 => RegionKind::Other, // seal
        21 => RegionKind::Table,
        22 | 23 => RegionKind::Text, // text / vertical_text
        24 => RegionKind::Footnote,  // vision_footnote
        _ => RegionKind::Other,
    }
}

/// A detected layout region in PDF user-space coordinates.
#[derive(Debug, Clone)]
pub struct Region {
    pub bbox: BBox,
    /// Raw model class id (DocStructBench for YOLO, 25-class for PPV2) — kept
    /// for debug; downstream uses [`Region::kind`].
    pub class: u8,
    pub kind: RegionKind,
    pub score: f32,
    /// Native reading-order key, when the model predicts one (PP-DocLayoutV2's
    /// `order_value`). `None` for DocLayout-YOLO → order via core XY-cut.
    pub order: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Yolo,
    Ppv2,
}

pub struct LayoutModel {
    model: Runnable,
    backend: Backend,
}

impl LayoutModel {
    pub fn new(model_path: &Path) -> Result<Self> {
        let bytes = std::fs::read(model_path)
            .with_context(|| format!("layout model at {}", model_path.display()))?;
        let raw = tract_onnx::onnx().model_for_read(&mut &bytes[..])?;
        // Auto-detect backend by input arity: DocLayout-YOLO has 1 input;
        // PP-DocLayoutV2 (RT-DETR) has 3 (im_shape, image, scale_factor).
        let backend = if raw.input_outlets()?.len() >= 3 {
            Backend::Ppv2
        } else {
            Backend::Yolo
        };
        let model = match backend {
            // YOLO: single dynamic input fixed to the detection canvas.
            Backend::Yolo => raw
                .with_input_fact(0, f32::fact([1, 3, SIDE, SIDE]).into())?
                .into_optimized()?
                .into_runnable()?,
            // PPV2: the simplified export already carries static shapes.
            Backend::Ppv2 => raw.into_optimized()?.into_runnable()?,
        };
        Ok(Self { model, backend })
    }

    /// Detect regions on a rendered page and map them back to PDF user space.
    /// `scale` is the raster scale (pixels per PDF point); `page_h` flips y.
    pub fn detect(
        &self,
        rgb: &[u8],
        w: usize,
        h: usize,
        scale: f32,
        page_h: f32,
    ) -> Result<Vec<Region>> {
        match self.backend {
            Backend::Yolo => self.detect_yolo(rgb, w, h, scale, page_h),
            Backend::Ppv2 => self.detect_ppv2(rgb, w, h, scale, page_h),
        }
    }

    /// PP-DocLayoutV2 (RT-DETR): exact resize to 800², `/255` (no mean/std),
    /// 3 inputs; output `[N,8]` = `[class, score, x1,y1,x2,y2, order, _]` with
    /// boxes already in rendered-pixel space. Native `order` is carried through.
    fn detect_ppv2(
        &self,
        rgb: &[u8],
        w: usize,
        h: usize,
        scale: f32,
        page_h: f32,
    ) -> Result<Vec<Region>> {
        const S: usize = PPV2_SIDE;
        let small = crate::resize_bilinear(rgb, w, h, S, S);
        let mut img = Tensor::zero::<f32>(&[1, 3, S, S])?;
        {
            let mut view = img.to_plain_array_view_mut::<f32>()?;
            let s = view.as_slice_mut().context("contiguous tensor")?;
            for c in 0..3 {
                for y in 0..S {
                    for x in 0..S {
                        s[c * S * S + y * S + x] = small[(y * S + x) * 3 + c] as f32 / 255.0;
                    }
                }
            }
        }
        let im_shape = Tensor::from_shape(&[1, 2], &[S as f32, S as f32])?;
        // scale_factor = target/orig (PaddleDetection convention); the graph
        // uses it to map boxes back to the rendered image's pixel space.
        let scale_factor =
            Tensor::from_shape(&[1, 2], &[S as f32 / h as f32, S as f32 / w as f32])?;
        // ONNX input order: [im_shape, image, scale_factor].
        let out = self
            .model
            .run(tvec!(im_shape.into(), img.into(), scale_factor.into()))?;
        let det = out[0].to_plain_array_view::<f32>()?;
        let shape = det.shape().to_vec();
        let (n, k) = (shape[0], shape[1]);
        let d = det.as_slice().context("det slice")?;

        let inv = 1.0 / scale; // rendered px → PDF pt
        let mut regions = Vec::new();
        for i in 0..n {
            let row = &d[i * k..(i + 1) * k];
            if row[1] < PPV2_SCORE_MIN {
                continue;
            }
            let class = row[0] as u8;
            let (x0, y0) = (row[2] * inv, row[3] * inv);
            let (x1, y1) = (row[4] * inv, row[5] * inv);
            regions.push(Region {
                bbox: BBox {
                    x0,
                    y0: page_h - y1,
                    x1,
                    y1: page_h - y0,
                },
                class,
                kind: map_ppv2(class),
                score: row[1],
                order: Some(row[6]),
            });
        }
        Ok(regions)
    }

    /// DocLayout-YOLO (YOLOv10, nms-free): letterbox into 1024² (gray 114),
    /// `/255`; decoded boxes carry no reading order (→ core XY-cut downstream).
    fn detect_yolo(
        &self,
        rgb: &[u8],
        w: usize,
        h: usize,
        scale: f32,
        page_h: f32,
    ) -> Result<Vec<Region>> {
        // Letterbox into SIDE² (gray 114 padding, YOLO convention).
        let r = (SIDE as f32 / w.max(h) as f32).min(1.0);
        let (sw, sh) = ((w as f32 * r) as usize, (h as f32 * r) as usize);
        let small = resize_nn(rgb, w, h, sw, sh);
        let (ox, oy) = ((SIDE - sw) / 2, (SIDE - sh) / 2);
        let mut t = Tensor::zero::<f32>(&[1, 3, SIDE, SIDE])?;
        {
            let mut view = t.to_plain_array_view_mut::<f32>()?;
            let s = view.as_slice_mut().context("contiguous tensor")?;
            s.fill(114.0 / 255.0);
            for c in 0..3 {
                for y in 0..sh {
                    for x in 0..sw {
                        s[c * SIDE * SIDE + (oy + y) * SIDE + (ox + x)] =
                            small[(y * sw + x) * 3 + c] as f32 / 255.0;
                    }
                }
            }
        }
        let out = self.model.run(tvec!(t.into()))?;
        let det = out[0].to_plain_array_view::<f32>()?;
        let shape = det.shape().to_vec();
        let (n, k) = (shape[1], shape[2]);
        let d = det.as_slice().context("det slice")?;

        let inv = 1.0 / r / scale; // canvas px → image px → PDF pt
        let mut regions = Vec::new();
        for i in 0..n {
            let row = &d[i * k..(i + 1) * k];
            if row[4] < SCORE_MIN {
                continue;
            }
            let (x0, y0) = ((row[0] - ox as f32) * inv, (row[1] - oy as f32) * inv);
            let (x1, y1) = ((row[2] - ox as f32) * inv, (row[3] - oy as f32) * inv);
            let class = row[5] as u8;
            regions.push(Region {
                // Pixel y runs top-down; PDF y runs bottom-up.
                bbox: BBox {
                    x0,
                    y0: page_h - y1,
                    x1,
                    y1: page_h - y0,
                },
                class,
                kind: map_yolo(class),
                score: row[4],
                order: None,
            });
        }
        Ok(regions)
    }
}

/// Tag the page's text chunks with region reading groups. Returns the new
/// element list, or `None` when there is nothing useful to do (few regions).
/// Regions are ordered by the core XY-cut over their boxes — the model picks
/// the regions, deterministic geometry picks their order.
/// Reading rank per region (`rank[i]` = position of `regions[i]`), computed
/// with the same XY-cut used for text, run over synthetic region chunks.
pub(crate) fn region_rank(page_no: usize, regions: &[Region]) -> Vec<u32> {
    // Native reading order (PP-DocLayoutV2's `order_value`): trust the model
    // when every region carries one — that is the whole point of adopting it.
    if !regions.is_empty() && regions.iter().all(|r| r.order.is_some()) {
        let mut idx: Vec<usize> = (0..regions.len()).collect();
        idx.sort_by(|&a, &b| {
            regions[a]
                .order
                .partial_cmp(&regions[b].order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut rank = vec![0u32; regions.len()];
        for (pos, &i) in idx.iter().enumerate() {
            rank[i] = pos as u32;
        }
        return rank;
    }
    let synthetic: Vec<TextChunk> = regions
        .iter()
        .map(|r| TextChunk {
            text: "r".into(),
            bbox: r.bbox,
            font_size: (r.bbox.y1 - r.bbox.y0).max(1.0),
            font: None,
            page: page_no,
            confidence: r.score,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        })
        .collect();
    let refs: Vec<&TextChunk> = synthetic.iter().collect();
    let order = reading_order(&refs);
    let mut rank = vec![0u32; regions.len()];
    for (pos, &idx) in order.iter().enumerate() {
        rank[idx] = pos as u32;
    }
    rank
}

pub fn assign_groups(page: &Page, regions: &[Region]) -> Option<Vec<Element>> {
    if regions.len() < MIN_REGIONS {
        return None;
    }
    let rank = region_rank(page.number, regions);

    let mut covered = 0usize;
    let mut total = 0usize;
    let elements: Vec<Element> = page
        .elements
        .iter()
        .map(|e| match e {
            Element::Text(t) => {
                let mut t = t.clone();
                total += 1;
                if let Some(i) = best_region(&t.bbox, regions) {
                    t.group = Some(rank[i]);
                    covered += 1;
                    // Flow the region's semantic role into the chunk (heading
                    // levels), unless the source already tagged it (G9a wins).
                    if t.tag.is_none() {
                        if let Some(role) = regions[i].kind.tag_role() {
                            t.tag = Some(role.to_string());
                        }
                    }
                }
                Element::Text(t)
            }
            other => other.clone(),
        })
        .collect();
    if total == 0 || (covered as f32 / total as f32) < MIN_COVERAGE {
        return None; // partial coverage scrambles more than it fixes
    }
    Some(elements)
}

/// The region containing most of the chunk (by center, then by overlap area);
/// `None` when the chunk touches no region (sorts after all groups).
fn best_region(b: &BBox, regions: &[Region]) -> Option<usize> {
    let (cx, cy) = ((b.x0 + b.x1) / 2.0, (b.y0 + b.y1) / 2.0);
    let mut best: Option<(usize, f32)> = None;
    for (i, r) in regions.iter().enumerate() {
        let rb = &r.bbox;
        let ix = (b.x1.min(rb.x1) - b.x0.max(rb.x0)).max(0.0);
        let iy = (b.y1.min(rb.y1) - b.y0.max(rb.y0)).max(0.0);
        let mut score = ix * iy;
        if cx >= rb.x0 && cx <= rb.x1 && cy >= rb.y0 && cy <= rb.y1 {
            score += 1.0; // containing the center wins ties against grazing overlaps
        }
        if score > 0.0 && best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((i, score));
        }
    }
    best.map(|(i, _)| i)
}

/// Fraction of sampled pixels darker than mid-gray.
fn dark_fraction(rgb: &[u8]) -> f32 {
    let mut dark = 0usize;
    let mut total = 0usize;
    // Sample every 97th pixel — plenty for a whole-page statistic.
    let mut i = 0;
    while i + 3 <= rgb.len() {
        let lum = rgb[i] as u32 + rgb[i + 1] as u32 + rgb[i + 2] as u32;
        if lum < 3 * 96 {
            dark += 1;
        }
        total += 1;
        i += 97 * 3;
    }
    if total == 0 {
        1.0
    } else {
        dark as f32 / total as f32
    }
}

fn resize_nn(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        for x in 0..dw {
            let sx = (x * sw / dw).min(sw - 1);
            let (s, d) = ((sy * sw + sx) * 3, (y * dw + x) * 3);
            out[d..d + 3].copy_from_slice(&src[s..s + 3]);
        }
    }
    out
}

/// Enhance a whole parsed PDF document: render each page on demand, detect
/// regions, and tag reading groups. Per-page failures (renderer/model) skip
/// that page — the deterministic result always stands. Returns the number of
/// pages that received groups.
pub fn enhance_document(
    doc: &mut docparse_core::ir::Document,
    pdf_bytes: Vec<u8>,
    model_path: &Path,
    scale: f32,
) -> Result<usize> {
    let raster = docparse_raster::Rasterizer::new(pdf_bytes)?;
    let model = LayoutModel::new(model_path)?;
    let mut enhanced = 0usize;
    for page in &mut doc.pages {
        let idx = page.number.saturating_sub(1);
        let (w, h, rgb) = match raster.render_rgb(idx, scale) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("layout: render failed on page {}: {e:#}", page.number);
                continue;
            }
        };
        // Broken-render guard: hayro (WIP upstream) occasionally produces a
        // mostly-black canvas (e.g. transparency-group bugs). Real document
        // pages are predominantly light; feeding a broken render to the model
        // yields garbage regions, so skip the page instead.
        if dark_fraction(&rgb) > BROKEN_RENDER_DARK_MAX {
            eprintln!(
                "layout: render of page {} looks broken (mostly dark) — skipping enhancement",
                page.number
            );
            continue;
        }
        let regions = match model.detect(&rgb, w as usize, h as usize, scale, page.height) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("layout: inference failed on page {}: {e:#}", page.number);
                continue;
            }
        };
        if std::env::var_os("DOCPARSE_LAYOUT_DEBUG").is_some() {
            eprintln!(
                "layout-debug: page {} render {}x{} pdf {}x{} regions={}",
                page.number,
                w,
                h,
                page.width,
                page.height,
                regions.len()
            );
            for r in regions.iter().take(6) {
                eprintln!(
                    "  class={} score={:.2} bbox=({:.0},{:.0})-({:.0},{:.0})",
                    r.class, r.score, r.bbox.x0, r.bbox.y0, r.bbox.x1, r.bbox.y1
                );
            }
        }
        // Group text by region (macro reading order), then seed empty Table
        // placeholders for detected table regions so `--table-model` (run
        // after) can recognize them. On image documents there are no vector
        // ruling lines for deterministic table detection, so without this the
        // model has nothing to refine; placeholders the model can't fill are
        // dropped by refine_tables.
        let grouped = assign_groups(page, &regions);
        let had_groups = grouped.is_some();
        // Take ownership (no extra clone of the element Vec / its pixel buffers
        // on the success path); only clone when grouping declined.
        let mut els = grouped.unwrap_or_else(|| page.elements.clone());
        let seeded = seed_table_regions(&mut els, &regions, page.number);
        if had_groups || seeded > 0 {
            page.elements = els;
            enhanced += 1;
        }
    }
    Ok(enhanced)
}

/// Append empty `Table` placeholders for table regions not already covered by
/// an existing table element. Returns how many were seeded. The placeholder's
/// empty `rows` signals `--table-model` to recognize it; if the model declines
/// or isn't run, refine_tables / output drop empty tables.
fn seed_table_regions(els: &mut Vec<Element>, regions: &[Region], page: usize) -> usize {
    let existing: Vec<BBox> = els
        .iter()
        .filter_map(|e| match e {
            Element::Table(t) => Some(t.bbox),
            _ => None,
        })
        .collect();
    let mut seeded = 0;
    for r in regions {
        if !r.kind.is_table() || r.score < SCORE_MIN {
            continue;
        }
        // Skip a region already covered by a deterministic table (>50% of the
        // region overlaps an existing table box).
        let ra = ((r.bbox.x1 - r.bbox.x0) * (r.bbox.y1 - r.bbox.y0)).max(1e-3);
        let covered = existing.iter().any(|b| {
            let ix = (r.bbox.x1.min(b.x1) - r.bbox.x0.max(b.x0)).max(0.0);
            let iy = (r.bbox.y1.min(b.y1) - r.bbox.y0.max(b.y0)).max(0.0);
            ix * iy / ra > 0.5
        });
        if covered {
            continue;
        }
        els.push(Element::Table(Table {
            bbox: r.bbox,
            page,
            rows: Vec::new(),
            source: Some("layout-region".to_string()),
        }));
        seeded += 1;
    }
    seeded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
        Region {
            bbox: BBox { x0, y0, x1, y1 },
            class: 1,
            kind: RegionKind::Text,
            score: 0.9,
            order: None,
        }
    }

    fn text_at(x0: f32, y0: f32, x1: f32, y1: f32) -> Element {
        Element::Text(TextChunk {
            text: "t".into(),
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
    fn groups_follow_region_reading_order() {
        // Two side-by-side column regions: left should be group 0, right 1.
        let regions = vec![
            region(300.0, 0.0, 580.0, 700.0),
            region(20.0, 0.0, 280.0, 700.0),
        ];
        let page = Page {
            number: 1,
            width: 600.0,
            height: 800.0,
            elements: vec![
                text_at(310.0, 650.0, 400.0, 660.0),
                text_at(30.0, 650.0, 120.0, 660.0),
            ],
        };
        let els = assign_groups(&page, &regions).unwrap();
        let groups: Vec<Option<u32>> = els
            .iter()
            .map(|e| match e {
                Element::Text(t) => t.group,
                _ => None,
            })
            .collect();
        // First element is in the RIGHT region → group 1; second in LEFT → 0.
        assert_eq!(groups, vec![Some(1), Some(0)]);
    }

    #[test]
    fn single_region_declines() {
        let page = Page {
            number: 1,
            width: 100.0,
            height: 100.0,
            elements: vec![text_at(0.0, 0.0, 10.0, 10.0)],
        };
        assert!(assign_groups(&page, &[region(0.0, 0.0, 100.0, 100.0)]).is_none());
    }

    fn table_region(x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
        Region {
            bbox: BBox { x0, y0, x1, y1 },
            class: 21,
            kind: RegionKind::Table,
            score: 0.9,
            order: None,
        }
    }

    #[test]
    fn seeds_placeholder_for_uncovered_table_region() {
        // A detected table region with no existing table → one empty
        // placeholder for --table-model to recognize.
        let mut els = vec![text_at(0.0, 0.0, 10.0, 10.0)];
        let seeded = seed_table_regions(&mut els, &[table_region(100.0, 100.0, 300.0, 300.0)], 1);
        assert_eq!(seeded, 1);
        let t = els.iter().find_map(|e| match e {
            Element::Table(t) => Some(t),
            _ => None,
        });
        assert!(t.is_some() && t.unwrap().rows.is_empty());
        assert_eq!(t.unwrap().source.as_deref(), Some("layout-region"));
    }

    #[test]
    fn skips_region_covered_by_existing_table_and_non_table_class() {
        let mut els = vec![Element::Table(Table {
            bbox: BBox {
                x0: 100.0,
                y0: 100.0,
                x1: 300.0,
                y1: 300.0,
            },
            page: 1,
            rows: vec![vec![]],
            source: None,
        })];
        // Same-area table region → covered, not seeded; a non-table region → ignored.
        let n = seed_table_regions(
            &mut els,
            &[
                table_region(105.0, 105.0, 295.0, 295.0),
                region(400.0, 400.0, 500.0, 500.0),
            ],
            1,
        );
        assert_eq!(n, 0);
    }
}

#[cfg(test)]
mod guard_tests {
    use super::dark_fraction;

    #[test]
    fn white_page_is_not_broken() {
        assert!(dark_fraction(&vec![250u8; 3000]) < 0.01);
    }

    #[test]
    fn black_canvas_is_broken() {
        assert!(dark_fraction(&vec![5u8; 3000]) > 0.9);
    }
}
