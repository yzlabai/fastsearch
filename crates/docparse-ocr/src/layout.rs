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
use docparse_core::ir::{BBox, Element, Page, TextChunk};
use docparse_core::reading_order::reading_order;
use std::path::Path;
use tract_onnx::prelude::*;

type Runnable = std::sync::Arc<TypedRunnableModel>;

/// Detection canvas (DocLayout-YOLO contract).
const SIDE: usize = 1024;
/// Keep regions at or above this score.
const SCORE_MIN: f32 = 0.25;
/// Skip enhancement when fewer regions than this (nothing to reorder).
const MIN_REGIONS: usize = 2;
/// Require at least this fraction of text chunks to land in a region before
/// grouping takes effect: partial coverage would push the uncovered majority
/// behind the covered minority and scramble the page.
const MIN_COVERAGE: f32 = 0.7;
/// A render whose pixels are mostly dark is assumed broken (document pages
/// are predominantly light). Sampled, cheap, conservative.
const BROKEN_RENDER_DARK_MAX: f32 = 0.4;

/// A detected layout region in PDF user-space coordinates.
#[derive(Debug, Clone)]
pub struct Region {
    pub bbox: BBox,
    pub class: u8,
    pub score: f32,
}

pub struct LayoutModel {
    model: Runnable,
}

impl LayoutModel {
    pub fn new(model_path: &Path) -> Result<Self> {
        let bytes = std::fs::read(model_path)
            .with_context(|| format!("layout model at {}", model_path.display()))?;
        let model = tract_onnx::onnx()
            .model_for_read(&mut &bytes[..])?
            .with_input_fact(0, f32::fact([1, 3, SIDE, SIDE]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model })
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
            regions.push(Region {
                // Pixel y runs top-down; PDF y runs bottom-up.
                bbox: BBox {
                    x0,
                    y0: page_h - y1,
                    x1,
                    y1: page_h - y0,
                },
                class: row[5] as u8,
                score: row[4],
            });
        }
        Ok(regions)
    }
}

/// Tag the page's text chunks with region reading groups. Returns the new
/// element list, or `None` when there is nothing useful to do (few regions).
/// Regions are ordered by the core XY-cut over their boxes — the model picks
/// the regions, deterministic geometry picks their order.
pub fn assign_groups(page: &Page, regions: &[Region]) -> Option<Vec<Element>> {
    if regions.len() < MIN_REGIONS {
        return None;
    }
    // Order regions with the same XY-cut used for text (synthetic chunks).
    let synthetic: Vec<TextChunk> = regions
        .iter()
        .map(|r| TextChunk {
            text: "r".into(),
            bbox: r.bbox,
            font_size: (r.bbox.y1 - r.bbox.y0).max(1.0),
            font: None,
            page: page.number,
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
    // rank[region_index] = reading position
    let mut rank = vec![0u32; regions.len()];
    for (pos, &idx) in order.iter().enumerate() {
        rank[idx] = pos as u32;
    }

    let mut covered = 0usize;
    let mut total = 0usize;
    let elements: Vec<Element> = page
        .elements
        .iter()
        .map(|e| match e {
            Element::Text(t) => {
                let mut t = t.clone();
                t.group = best_region(&t.bbox, regions).map(|i| rank[i]);
                total += 1;
                if t.group.is_some() {
                    covered += 1;
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
        if let Some(els) = assign_groups(page, &regions) {
            page.elements = els;
            enhanced += 1;
        }
    }
    Ok(enhanced)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x0: f32, y0: f32, x1: f32, y1: f32) -> Region {
        Region {
            bbox: BBox { x0, y0, x1, y1 },
            class: 1,
            score: 0.9,
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
