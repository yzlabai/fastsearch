//! ONNX-embedded OCR enhancer — the N3/P4 route: PP-OCR det+rec models
//! (any generation — v4/v5 from RapidOCR, v6 from PaddlePaddle) running on
//! `tract`, a pure-Rust inference runtime. No Python, no C++, no subprocess;
//! models are *external files* loaded at runtime, so the core stays model-free
//! and the deterministic path never touches this crate (cost thesis: only
//! quality-flagged pages route here). Default is PP-OCRv6 tiny.
//!
//! Pipeline (mirrors RapidOCR's, independently implemented):
//!   page image (extracted XObject pixels, never rasterized)
//!   → det (DBNet, 960×960 padded canvas) → threshold → connected components
//!   → boxes (rect offset ≈ DB unclip) → per-box crop, resize h=48, width
//!   buckets → rec (SVTR-LCNet) → CTC greedy decode + mean-prob confidence
//!   → TextChunks in PDF user space (pixel coords mapped through the image's
//!   placement bbox), `source: "ocr:ppocr"`, confidence < 1.
//!
//! Model files (see docs/plans/n3-real-enhancer.md): a `*det*.onnx`, a
//! `*rec*.onnx`, and the char dict (either a `*dict*.txt`, or `character_dict`
//! parsed from a `*rec*.yml` for PP-OCRv6). paddle2onnx dim names contain dots
//! tract can't parse (sanitized in-memory), and PP-OCRv6's export bakes a
//! symbolic batch dim into intermediate shapes that tract rejects — both are
//! handled at load (see `onnx_loader` / `load_dict`), so the raw HuggingFace
//! ONNX loads with no offline prep step.
//!
//! Orientation (H2): the scan buffer is first rotated to viewing orientation
//! (`ImageChunk::turns`, recorded by the PDF backend from /Rotate or a rotated
//! CTM), then page-level text rotation is detected and undone — 90/270 via the
//! det-box aspect vote (vertical line strips), 180 via the PP-OCR cls model
//! (optional file `*cls*.onnx`, 0/180 line classifier). Without cls ALL
//! rotation correction is disabled — a 90° fix that can't be disambiguated
//! from 270° is a coin flip, worse than declining. Emitted bboxes are mapped
//! back to viewing space so citations stay viewer-faithful.

pub mod formula;
pub mod layout;
pub mod table_model;
pub mod transcribe;
pub mod unirec;

use anyhow::{Context, Result};
use docparse_core::enhance::{Capability, Enhancer};
use docparse_core::ir::{BBox, Element, ImageChunk, ImageKind, Page, TextChunk};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tract_onnx::prelude::*;

type Runnable = std::sync::Arc<TypedRunnableModel>;

/// Detection canvas side: keep-ratio resize into a fixed square so the det
/// model is optimized exactly once. 960 is RapidOCR's default max side.
const DET_SIDE: usize = 960;
/// DBNet probability threshold for "this pixel is text".
const DET_THRESHOLD: f32 = 0.3;
/// Approximate DB unclip: expand each box by `area * RATIO / perimeter` on
/// every side (rectangular Vatti offset), matching RapidOCR's 1.6 ratio.
const UNCLIP_RATIO: f32 = 1.6;
/// Recognizer input height (PP-OCR rec contract).
const REC_HEIGHT: usize = 48;
/// Width buckets for the recognizer: one optimized model per bucket, cached.
const REC_BUCKETS: [usize; 6] = [80, 160, 320, 480, 640, 960];
/// Drop rec results below this mean CTC probability (noise crops).
const MIN_CONFIDENCE: f32 = 0.35;
/// cls model input (PP-OCR text-line orientation classifier contract).
const CLS_HEIGHT: usize = 48;
const CLS_WIDTH: usize = 192;
/// A line crop votes "180°" only above this cls probability.
const CLS_THRESHOLD: f32 = 0.6;
/// How many line crops vote on the page's 180° orientation.
const CLS_SAMPLES: usize = 8;

pub struct PpOcrEnhancer {
    det: Runnable,
    rec_bytes: Vec<u8>,
    rec_cache: Mutex<HashMap<usize, Runnable>>,
    dict: Vec<String>,
    /// Optional 0/180 line-orientation classifier (`*cls*.onnx`).
    cls: Option<Runnable>,
}

impl PpOcrEnhancer {
    /// Load models from a directory. Fails with a traceable error when files
    /// are missing — callers decide whether OCR is optional.
    pub fn new(model_dir: &Path) -> Result<Self> {
        // Accept any PP-OCR generation: the v4 exact file names, or generic
        // ones (*det*.onnx / *rec*.onnx, dict via *dict*.txt or a *rec*.yml) so
        // a v5 or v6 model set drops in without renames or offline prep.
        let det_path = find_file(model_dir, &["ch_PP-OCRv4_det_infer.onnx"], "det", ".onnx")?;
        let rec_path = find_file(model_dir, &["ch_PP-OCRv4_rec_infer.onnx"], "rec", ".onnx")?;
        let det_bytes = sanitize_dims(
            &std::fs::read(&det_path)
                .with_context(|| format!("det model {}", det_path.display()))?,
        );
        let rec_bytes = sanitize_dims(
            &std::fs::read(&rec_path)
                .with_context(|| format!("rec model {}", rec_path.display()))?,
        );
        let dict = load_dict(model_dir)?;
        anyhow::ensure!(
            dict.len() > 6000,
            "dictionary looks truncated: {} entries",
            dict.len()
        );

        let det = onnx_loader()
            .model_for_read(&mut &det_bytes[..])?
            .with_input_fact(0, f32::fact([1, 3, DET_SIDE, DET_SIDE]).into())?
            .into_optimized()?
            .into_runnable()?;
        // Orientation classifier is optional: rotation correction degrades
        // gracefully (no 180° detection) when the file isn't there.
        let cls = find_file(
            model_dir,
            &["ch_ppocr_mobile_v2.0_cls_infer.onnx"],
            "cls",
            ".onnx",
        )
        .ok()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|bytes| {
            let bytes = sanitize_dims(&bytes);
            onnx_loader()
                .model_for_read(&mut &bytes[..])
                .ok()?
                .with_input_fact(0, f32::fact([1, 3, CLS_HEIGHT, CLS_WIDTH]).into())
                .ok()?
                .into_optimized()
                .ok()?
                .into_runnable()
                .ok()
        });
        Ok(Self {
            det,
            rec_bytes,
            rec_cache: Mutex::new(HashMap::new()),
            dict,
            cls,
        })
    }

    /// Text-line detection: boxes in original pixel coordinates.
    fn det_boxes(&self, rgb: &[u8], w: usize, h: usize) -> Result<Vec<[usize; 4]>> {
        // ---- det: keep-ratio resize into the fixed canvas, imagenet norm ----
        let ratio = (DET_SIDE as f32 / w.max(h) as f32).min(1.0);
        let (sw, sh) = (
            ((w as f32 * ratio) as usize).max(32),
            ((h as f32 * ratio) as usize).max(32),
        );
        let small = resize_bilinear(rgb, w, h, sw, sh);
        let mean = [0.485f32, 0.456, 0.406];
        let std = [0.229f32, 0.224, 0.225];
        let mut t = Tensor::zero::<f32>(&[1, 3, DET_SIDE, DET_SIDE])?;
        {
            let mut view = t.to_plain_array_view_mut::<f32>()?;
            let s = view.as_slice_mut().context("contiguous tensor")?;
            for c in 0..3 {
                for y in 0..sh {
                    for x in 0..sw {
                        let v = small[(y * sw + x) * 3 + c] as f32 / 255.0;
                        s[c * DET_SIDE * DET_SIDE + y * DET_SIDE + x] = (v - mean[c]) / std[c];
                    }
                }
            }
            // Padding stays 0 — below threshold after normalization for det.
        }
        let out = self.det.run(tvec!(t.into()))?;
        let prob = out[0].to_plain_array_view::<f32>()?;
        let map = prob.as_slice().context("det prob map")?;

        // ---- boxes from the thresholded map (within the valid sw×sh region) ----
        let boxes = component_boxes(map, DET_SIDE, sw, sh, DET_THRESHOLD);
        if std::env::var_os("DOCPARSE_OCR_DEBUG").is_some() {
            let hits = map.iter().filter(|&&p| p > DET_THRESHOLD).count();
            eprintln!(
                "ocr-debug: det region {sw}x{sh}, {hits} px>thr, {} boxes",
                boxes.len()
            );
        }

        // Unclip and scale back to original pixel coords. ALL edges are
        // clamped — the canvas floor (`.max(32)`) can stretch `inv` so a
        // canvas-valid box maps past the image, and downstream consumers
        // (orient's aspect vote, rotate_box) rely on x0<=x1<=w / y0<=y1<=h.
        let inv = 1.0 / ratio;
        Ok(boxes
            .into_iter()
            .map(|b| {
                let [bx0, by0, bx1, by1] = unclip(b, sw, sh);
                [
                    ((bx0 as f32 * inv) as usize).min(w),
                    ((by0 as f32 * inv) as usize).min(h),
                    ((bx1 as f32 * inv) as usize).min(w),
                    ((by1 as f32 * inv) as usize).min(h),
                ]
            })
            .collect())
    }

    /// Recognize each detected box (top-to-bottom; layout re-orders downstream).
    fn ocr_boxes(
        &self,
        rgb: &[u8],
        w: usize,
        _h: usize,
        boxes: &[[usize; 4]],
    ) -> Result<Vec<(String, [usize; 4], f32)>> {
        let mut results = Vec::new();
        for &[ox0, oy0, ox1, oy1] in boxes {
            let (cw, ch) = (ox1.saturating_sub(ox0), oy1.saturating_sub(oy0));
            if cw < 4 || ch < 4 {
                continue;
            }
            let mut crop = vec![0u8; cw * ch * 3];
            for y in 0..ch {
                let src = ((oy0 + y) * w + ox0) * 3;
                crop[y * cw * 3..(y + 1) * cw * 3].copy_from_slice(&rgb[src..src + cw * 3]);
            }
            if let Some((text, conf)) = self.recognize(&crop, cw, ch)? {
                if std::env::var_os("DOCPARSE_OCR_DEBUG").is_some() {
                    eprintln!(
                        "ocr-debug: box {cw}x{ch}@({ox0},{oy0}) conf={conf:.2} | {}",
                        &text.chars().take(40).collect::<String>()
                    );
                }
                if conf >= MIN_CONFIDENCE && !text.trim().is_empty() {
                    results.push((text, [ox0, oy0, ox1, oy1], conf));
                }
            }
        }
        Ok(results)
    }

    /// Detect and undo page-level text rotation: 90/270 by det-box aspect
    /// vote (a rotated page's text lines detect as tall vertical strips),
    /// then 180 by cls vote — which also disambiguates 90 from 270. Returns
    /// the upright buffer, its dims, det boxes on it, and the CW quarter-turns
    /// applied (so callers can map bboxes back).
    #[allow(clippy::type_complexity)]
    fn orient(
        &self,
        rgb: Vec<u8>,
        w: usize,
        h: usize,
    ) -> Result<(Vec<u8>, usize, usize, Vec<[usize; 4]>, u8)> {
        let mut boxes = self.det_boxes(&rgb, w, h)?;
        let (mut buf, mut bw, mut bh, mut turns) = (rgb, w, h, 0u8);
        let tall = boxes
            .iter()
            .filter(|b| b[3].saturating_sub(b[1]) as f32 > 1.5 * b[2].saturating_sub(b[0]) as f32)
            .count();
        let wide = boxes
            .iter()
            .filter(|b| b[2].saturating_sub(b[0]) as f32 > 1.5 * b[3].saturating_sub(b[1]) as f32)
            .count();
        // The 90° fix is gated on cls being available: without it, 90 vs 270
        // is a coin flip, and rotating the wrong way replaces the page with
        // upside-down garbage — strictly worse than leaving it (2026-06-11
        // review). Without cls the page stays as-is (pre-H2 behavior).
        if self.cls.is_some() && tall >= 2 && tall > wide {
            buf = rotate_rgb(&buf, bw, bh, 1);
            (bw, bh) = (bh, bw);
            turns = 1;
            // Det boxes on vertical text are coarser than on horizontal text;
            // redo det on the upright image for clean line boxes.
            boxes = self.det_boxes(&buf, bw, bh)?;
        }
        if self.upside_down(&buf, bw, bh, &boxes)? {
            buf = rotate_rgb(&buf, bw, bh, 2);
            boxes = boxes.iter().map(|&b| rotate_box(b, bw, bh, 2)).collect();
            turns = (turns + 2) % 4;
        }
        if std::env::var_os("DOCPARSE_OCR_DEBUG").is_some() && turns != 0 {
            eprintln!("ocr-debug: orientation corrected by {}°", turns as u32 * 90);
        }
        Ok((buf, bw, bh, boxes, turns))
    }

    /// Majority cls vote over the widest line crops. `false` when the cls
    /// model is absent or there are too few proper line boxes to trust a vote
    /// (a wrong 180° flip is worse than the status quo).
    fn upside_down(&self, rgb: &[u8], w: usize, _h: usize, boxes: &[[usize; 4]]) -> Result<bool> {
        let Some(cls) = &self.cls else {
            return Ok(false);
        };
        let mut lines: Vec<[usize; 4]> = boxes
            .iter()
            .copied()
            .filter(|b| {
                let (bw, bh) = (b[2].saturating_sub(b[0]), b[3].saturating_sub(b[1]));
                bw >= 2 * bh && bh >= 8
            })
            .collect();
        lines.sort_by_key(|b| std::cmp::Reverse((b[2] - b[0]) * (b[3] - b[1])));
        lines.truncate(CLS_SAMPLES);
        if lines.len() < 2 {
            return Ok(false);
        }
        // Majority threshold; exit early once the outcome is decided either way.
        let need = lines.len() / 2 + 1;
        let mut votes = 0usize;
        for (i, &[x0, y0, x1, y1]) in lines.iter().enumerate() {
            if votes >= need || votes + (lines.len() - i) < need {
                break;
            }
            let (cw, ch) = (x1 - x0, y1 - y0);
            let mut crop = vec![0u8; cw * ch * 3];
            for y in 0..ch {
                let src = ((y0 + y) * w + x0) * 3;
                crop[y * cw * 3..(y + 1) * cw * 3].copy_from_slice(&rgb[src..src + cw * 3]);
            }
            // cls contract: resize h=48, keep ratio capped at w=192, pad black,
            // normalize (x/255 - 0.5)/0.5 — same scheme as rec.
            let rw = ((cw as f32 * (CLS_HEIGHT as f32 / ch as f32)) as usize).clamp(16, CLS_WIDTH);
            let resized = resize_bilinear(&crop, cw, ch, rw, CLS_HEIGHT);
            let mut t = Tensor::zero::<f32>(&[1, 3, CLS_HEIGHT, CLS_WIDTH])?;
            {
                let mut view = t.to_plain_array_view_mut::<f32>()?;
                let s = view.as_slice_mut().context("contiguous tensor")?;
                for c in 0..3 {
                    for y in 0..CLS_HEIGHT {
                        for x in 0..rw {
                            let v = resized[(y * rw + x) * 3 + c] as f32 / 255.0;
                            s[c * CLS_HEIGHT * CLS_WIDTH + y * CLS_WIDTH + x] = (v - 0.5) / 0.5;
                        }
                    }
                }
            }
            let out = cls.run(tvec!(t.into()))?;
            let probs = out[0].to_plain_array_view::<f32>()?;
            let p = probs.as_slice().context("cls probs")?;
            // Output is [1, 2] softmax over labels ["0", "180"].
            if p.len() >= 2 && p[1] > CLS_THRESHOLD {
                votes += 1;
            }
        }
        Ok(votes >= need)
    }

    /// Run the recognizer on one cropped line image.
    fn recognize(&self, rgb: &[u8], w: usize, h: usize) -> Result<Option<(String, f32)>> {
        let rw = ((w as f32 * (REC_HEIGHT as f32 / h as f32)) as usize).max(16);
        let bucket = match REC_BUCKETS.iter().find(|&&b| b >= rw) {
            Some(&b) => b,
            None => *REC_BUCKETS.last().unwrap(),
        };
        let rw = rw.min(bucket);
        let resized = resize_bilinear(rgb, w, h, rw, REC_HEIGHT);

        let mut cache = self.rec_cache.lock().unwrap();
        let model = match cache.entry(bucket) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(
                onnx_loader()
                    .model_for_read(&mut &self.rec_bytes[..])?
                    .with_input_fact(0, f32::fact([1, 3, REC_HEIGHT, bucket]).into())?
                    .into_optimized()?
                    .into_runnable()?,
            ),
        };

        let mut t = Tensor::zero::<f32>(&[1, 3, REC_HEIGHT, bucket])?;
        {
            let mut view = t.to_plain_array_view_mut::<f32>()?;
            let s = view.as_slice_mut().context("contiguous tensor")?;
            for c in 0..3 {
                for y in 0..REC_HEIGHT {
                    for x in 0..rw {
                        let v = resized[(y * rw + x) * 3 + c] as f32 / 255.0;
                        s[c * REC_HEIGHT * bucket + y * bucket + x] = (v - 0.5) / 0.5;
                    }
                }
            }
        }
        let out = model.run(tvec!(t.into()))?;
        let logits = out[0].to_plain_array_view::<f32>()?;
        let shape = logits.shape().to_vec();
        let (steps, classes) = (shape[1], shape[2]);
        Ok(Some(ctc_greedy(
            logits.as_slice().context("logits")?,
            steps,
            classes,
            &self.dict,
        )))
    }
}

impl Enhancer for PpOcrEnhancer {
    fn capability(&self) -> Capability {
        Capability {
            name: "ppocr-onnx".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            handles_scanned: true,
            handles_garbled: false,
        }
    }

    /// OCR the page's scan image (the page-covering `ImageChunk` the PDF
    /// backend attached pixels to). Returns the page with one `TextChunk` per
    /// recognized line appended — orientation-normalized when the scan was
    /// rotated — or `None` when there's nothing usable.
    fn enhance_page(&self, page: &Page) -> Option<Page> {
        let img = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Image(i) if i.kind != ImageKind::None && !i.data.is_empty() => Some(i),
                _ => None,
            })
            .max_by(|a, b| {
                area(a)
                    .partial_cmp(&area(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;

        let (w, h) = (img.width_px as usize, img.height_px as usize);
        let rgb = to_rgb(img)?;
        // Bring the buffer to viewing orientation first (page /Rotate or a
        // rotated placement, recorded as `ImageChunk::turns`), then detect and
        // undo page-level text rotation — one mechanism covers both a
        // /Rotate'd upright scan and a physically rotated one.
        let vturns = img.turns % 4;
        let (vbuf, vw, vh) = match vturns {
            0 => (rgb, w, h),
            t => {
                let b = rotate_rgb(&rgb, w, h, t);
                drop(rgb); // scan buffers are ~100MB; don't hold both copies
                if t == 2 {
                    (b, w, h)
                } else {
                    (b, h, w)
                }
            }
        };
        // Surface inference errors instead of silently declining (they mean a
        // broken model/setup, not a hard page) — then decline so the
        // deterministic result still stands.
        let fail = |e: anyhow::Error| {
            eprintln!(
                "ppocr-onnx: inference failed on page {}: {e:#}",
                page.number
            );
        };
        let (ubuf, uw, uh, boxes, dturns) = match self.orient(vbuf, vw, vh) {
            Ok(v) => v,
            Err(e) => {
                fail(e);
                return None;
            }
        };
        let lines = match self.ocr_boxes(&ubuf, uw, uh, &boxes) {
            Ok(l) => l,
            Err(e) => {
                fail(e);
                return None;
            }
        };
        if lines.is_empty() {
            return None;
        }

        // Mixed pages (G4): the deterministic text layer wins — drop OCR
        // results that substantially overlap existing visible text so the
        // page never carries the same words twice.
        let existing: Vec<docparse_core::ir::BBox> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) if !t.hidden => Some(t.bbox),
                _ => None,
            })
            .collect();

        // A pure scan page that needed rotation is orientation-NORMALIZED:
        // the output page is the upright content (Docling's convention) —
        // reading order and citations refer to the corrected page, and for
        // 90/270 the page dimensions swap. Mixed pages stay viewer-faithful
        // (the deterministic text anchors the viewing frame there).
        let normalize = dturns != 0 && existing.is_empty();
        let (page_w, page_h) = if normalize && dturns % 2 == 1 {
            (page.height, page.width)
        } else {
            (page.width, page.height)
        };
        let mut elements: Vec<Element> = if normalize {
            page.elements
                .iter()
                .cloned()
                .map(|mut e| {
                    match &mut e {
                        Element::Text(t) => {
                            t.bbox = rotate_pdf_bbox(t.bbox, page.width, page.height, dturns)
                        }
                        Element::Image(i) => {
                            i.bbox = rotate_pdf_bbox(i.bbox, page.width, page.height, dturns);
                            i.turns = (i.turns + dturns) % 4;
                        }
                        Element::Table(t) => {
                            t.bbox = rotate_pdf_bbox(t.bbox, page.width, page.height, dturns)
                        }
                    }
                    e
                })
                .collect()
        } else {
            page.elements.clone()
        };
        let ibox = if normalize {
            rotate_pdf_bbox(img.bbox, page.width, page.height, dturns)
        } else {
            img.bbox
        };
        let (bw, bh) = (ibox.x1 - ibox.x0, ibox.y1 - ibox.y0);
        // Normalized output keeps boxes in the upright OCR frame; otherwise
        // map them back to the viewing frame (where the placement bbox lives).
        let (back, fw, fh) = if normalize {
            (0, uw, uh)
        } else {
            ((4 - dturns) % 4, vw, vh)
        };
        for (text, pbox, conf) in lines {
            let [px0, py0, px1, py1] = rotate_box(pbox, uw, uh, back);
            let x0 = ibox.x0 + px0 as f32 / fw as f32 * bw;
            let x1 = ibox.x0 + px1 as f32 / fw as f32 * bw;
            // Pixel y runs top-down; PDF y runs bottom-up.
            let y1 = ibox.y1 - py0 as f32 / fh as f32 * bh;
            let y0 = ibox.y1 - py1 as f32 / fh as f32 * bh;
            let ocr_box = BBox { x0, y0, x1, y1 };
            if overlaps_existing(&ocr_box, &existing) {
                continue;
            }
            elements.push(Element::Text(TextChunk {
                text,
                bbox: BBox { x0, y0, x1, y1 },
                font_size: (y1 - y0).max(1.0),
                font: None,
                page: page.number,
                // Model output is never fully trusted: cap below 1.0 so
                // downstream can always tell OCR from deterministic text.
                confidence: conf.min(0.99),
                bold: false,
                hidden: false,
                source: Some("ocr:ppocr".into()),
                group: None,
                tag: None,
            }));
        }
        Some(Page {
            number: page.number,
            width: page_w,
            height: page_h,
            elements,
        })
    }
}

/// Rotate a PDF-space bbox (y up) by CW quarter-turns of the page: one turn
/// maps a point (x, y) on a (w, h) page to (y, w − x) on an (h, w) page.
fn rotate_pdf_bbox(b: BBox, w: f32, h: f32, turns: u8) -> BBox {
    let (mut b, mut w, mut h) = (b, w, h);
    for _ in 0..(turns % 4) {
        b = BBox {
            x0: b.y0,
            y0: w - b.x1,
            x1: b.y1,
            y1: w - b.x0,
        };
        std::mem::swap(&mut w, &mut h);
    }
    let _ = h;
    b
}

/// Rotate an RGB buffer by CW quarter-turns. 1 and 3 swap the dimensions.
pub(crate) fn rotate_rgb(src: &[u8], w: usize, h: usize, turns: u8) -> Vec<u8> {
    let mut dst = vec![0u8; src.len()];
    match turns % 4 {
        0 => dst.copy_from_slice(src),
        2 => {
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 3;
                    let t = ((h - 1 - y) * w + (w - 1 - x)) * 3;
                    dst[t..t + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
        }
        // 90 CW: dst is (h, w); src (x, y) lands at (h-1-y, x).
        1 => {
            let dw = h;
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 3;
                    let t = (x * dw + (h - 1 - y)) * 3;
                    dst[t..t + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
        }
        // 270 CW (= 90 CCW): dst is (h, w); src (x, y) lands at (y, w-1-x).
        _ => {
            let dw = h;
            for y in 0..h {
                for x in 0..w {
                    let s = (y * w + x) * 3;
                    let t = ((w - 1 - x) * dw + y) * 3;
                    dst[t..t + 3].copy_from_slice(&src[s..s + 3]);
                }
            }
        }
    }
    dst
}

/// Map a pixel box (x0, y0, x1, y1; max-exclusive edges) from a (w, h) frame
/// into the frame rotated `turns` quarter-turns clockwise.
pub(crate) fn rotate_box(b: [usize; 4], w: usize, h: usize, turns: u8) -> [usize; 4] {
    let (mut b, mut w, mut h) = (b, w, h);
    for _ in 0..(turns % 4) {
        b = [h - b[3], b[0], h - b[1], b[2]];
        std::mem::swap(&mut w, &mut h);
    }
    let _ = (w, h);
    b
}

/// Whether more than half of `b`'s area is covered by any existing text box.
fn overlaps_existing(b: &BBox, existing: &[BBox]) -> bool {
    let area = ((b.x1 - b.x0) * (b.y1 - b.y0)).max(1e-6);
    existing.iter().any(|e| {
        let ix = (b.x1.min(e.x1) - b.x0.max(e.x0)).max(0.0);
        let iy = (b.y1.min(e.y1) - b.y0.max(e.y0)).max(0.0);
        ix * iy / area > 0.5
    })
}

fn area(i: &ImageChunk) -> f32 {
    (i.bbox.x1 - i.bbox.x0) * (i.bbox.y1 - i.bbox.y0)
}

/// Materialize RGB bytes from an image payload (Gray expanded, JPEG decoded).
fn to_rgb(img: &ImageChunk) -> Option<Vec<u8>> {
    let (w, h) = (img.width_px as usize, img.height_px as usize);
    match img.kind {
        ImageKind::Rgb8 if img.data.len() == w * h * 3 => Some(img.data.clone()),
        ImageKind::Gray8 if img.data.len() == w * h => {
            Some(img.data.iter().flat_map(|&g| [g, g, g]).collect())
        }
        ImageKind::Jpeg => {
            let mut dec = zune_jpeg::JpegDecoder::new(&img.data);
            let pixels = dec.decode().ok()?;
            let cs = dec.get_output_colorspace()?;
            match cs.num_components() {
                3 if pixels.len() == w * h * 3 => Some(pixels),
                1 if pixels.len() == w * h => {
                    Some(pixels.iter().flat_map(|&g| [g, g, g]).collect())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// paddle2onnx names dynamic dims `p2o.DynamicDimension.N` (older exports) or
/// `DynamicDimension.N` (newer, e.g. PP-OCRv5); tract's symbol parser rejects
/// the dots. Equal-length byte patches keep the protobuf intact.
fn sanitize_dims(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    replace_inplace(&mut out, b"p2o.DynamicDimension.", b"p2o_DynamicDimension_");
    replace_inplace(&mut out, b"DynamicDimension.", b"DynamicDimension_");
    out
}

/// ONNX loader configured to ignore the graph's baked-in `value_info`.
///
/// PaddleOCR's PP-OCRv6 export annotates intermediate nodes with a symbolic
/// batch dim, which tract refuses to unify with the concrete batch=1 we pin via
/// `with_input_fact` (`Impossible to unify Sym(DynamicDimension_0) with Val(1)`).
/// Ignoring `value_info` makes tract re-infer every intermediate shape from the
/// pinned input — equivalent to the old `prepare.py` static-ization, but with
/// zero Python: the raw HuggingFace ONNX loads as-is. Shapes are fully
/// inferable for DB det / SVTR rec / cls, so this is safe across PP-OCRv4/v5/v6
/// (verified: v4 chinese_scan unchanged, v6 raw == onnxruntime).
fn onnx_loader() -> tract_onnx::Onnx {
    tract_onnx::onnx().with_ignore_value_info(true)
}

/// Load the recognition character dict: the flat `*dict*.txt` if present, else
/// parse `character_dict` out of a PP-OCR `*rec*.yml` / `*.yml` (PP-OCRv6 ships
/// the dict only inside inference.yml, so this avoids a Python extraction step).
fn load_dict(dir: &Path) -> Result<Vec<String>> {
    if let Ok(txt) = find_file(dir, &["ppocr_keys_v1.txt"], "dict", ".txt") {
        return Ok(std::fs::read_to_string(&txt)
            .with_context(|| format!("dictionary {}", txt.display()))?
            .lines()
            .map(str::to_owned)
            .collect());
    }
    let yml = find_file(dir, &[], "rec", ".yml")
        .or_else(|_| find_file(dir, &[], "", ".yml"))
        .context("no *dict*.txt and no *.yml to extract character_dict from")?;
    let text =
        std::fs::read_to_string(&yml).with_context(|| format!("rec yml {}", yml.display()))?;
    parse_yml_char_dict(&text).with_context(|| format!("character_dict in {}", yml.display()))
}

/// Parse the `character_dict:` block list from a PP-OCR rec inference.yml.
///
/// The block is a constrained YAML list — `  - <scalar>` lines, one char each,
/// either single-quoted (`'x'`, with `''` escaping a literal quote) or bare —
/// so a full YAML parser is overkill. Reads from `character_dict:` until the
/// indentation drops below the list items.
fn parse_yml_char_dict(text: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        if !in_block {
            if line.trim_end() == "character_dict:" || line.trim_start() == "character_dict:" {
                in_block = true;
            }
            continue;
        }
        let trimmed = line.trim_start();
        let Some(item) = trimmed.strip_prefix("- ") else {
            // A non-list line at this point ends the block (next yml key).
            if trimmed.is_empty() {
                continue;
            }
            break;
        };
        let item = item.trim();
        let unquoted =
            if let Some(inner) = item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
                inner.replace("''", "'")
            } else {
                item.to_string()
            };
        out.push(unquoted);
    }
    anyhow::ensure!(!out.is_empty(), "character_dict block empty or not found");
    Ok(out)
}

/// Resolve a model file: exact known names first, then any file whose name
/// contains `needle` with the given extension (alphabetically first match).
pub(crate) fn find_file(
    dir: &Path,
    exact: &[&str],
    needle: &str,
    ext: &str,
) -> Result<std::path::PathBuf> {
    for name in exact {
        let p = dir.join(name);
        if p.exists() {
            return Ok(p);
        }
    }
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("model dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let l = n.to_ascii_lowercase();
                    l.contains(needle) && l.ends_with(ext)
                })
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .with_context(|| format!("no *{needle}*{ext} in {}", dir.display()))
}

fn replace_inplace(buf: &mut [u8], pat: &[u8], rep: &[u8]) {
    debug_assert_eq!(pat.len(), rep.len());
    let mut i = 0;
    while i + pat.len() <= buf.len() {
        if &buf[i..i + pat.len()] == pat {
            buf[i..i + rep.len()].copy_from_slice(rep);
        }
        i += 1;
    }
}

/// Bilinear RGB resize.
pub(crate) fn resize_bilinear(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 3];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for y in 0..dh {
        let syf = ((y as f32 + 0.5) * fy - 0.5).max(0.0);
        let sy = (syf as usize).min(sh - 1);
        let sy1 = (sy + 1).min(sh - 1);
        let wy = syf - sy as f32;
        for x in 0..dw {
            let sxf = ((x as f32 + 0.5) * fx - 0.5).max(0.0);
            let sx = (sxf as usize).min(sw - 1);
            let sx1 = (sx + 1).min(sw - 1);
            let wx = sxf - sx as f32;
            for c in 0..3 {
                let p00 = src[(sy * sw + sx) * 3 + c] as f32;
                let p01 = src[(sy * sw + sx1) * 3 + c] as f32;
                let p10 = src[(sy1 * sw + sx) * 3 + c] as f32;
                let p11 = src[(sy1 * sw + sx1) * 3 + c] as f32;
                let v = p00 * (1.0 - wx) * (1.0 - wy)
                    + p01 * wx * (1.0 - wy)
                    + p10 * (1.0 - wx) * wy
                    + p11 * wx * wy;
                out[(y * dw + x) * 3 + c] = (v + 0.5) as u8;
            }
        }
    }
    out
}

/// Connected components over the thresholded probability map (4-neighbour
/// flood fill), returning pixel-space bounding boxes within the valid region.
fn component_boxes(
    map: &[f32],
    stride: usize,
    sw: usize,
    sh: usize,
    threshold: f32,
) -> Vec<[usize; 4]> {
    let mut seen = vec![false; sw * sh];
    let mut boxes = Vec::new();
    let mut stack = Vec::new();
    for y0 in 0..sh {
        for x0 in 0..sw {
            if seen[y0 * sw + x0] || map[y0 * stride + x0] <= threshold {
                continue;
            }
            let (mut minx, mut miny, mut maxx, mut maxy) = (x0, y0, x0, y0);
            let mut count = 0usize;
            stack.push((x0, y0));
            seen[y0 * sw + x0] = true;
            while let Some((x, y)) = stack.pop() {
                count += 1;
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
                let mut push = |nx: usize, ny: usize, stack: &mut Vec<(usize, usize)>| {
                    if !seen[ny * sw + nx] && map[ny * stride + nx] > threshold {
                        seen[ny * sw + nx] = true;
                        stack.push((nx, ny));
                    }
                };
                if x > 0 {
                    push(x - 1, y, &mut stack);
                }
                if x + 1 < sw {
                    push(x + 1, y, &mut stack);
                }
                if y > 0 {
                    push(x, y - 1, &mut stack);
                }
                if y + 1 < sh {
                    push(x, y + 1, &mut stack);
                }
            }
            // Noise gate: tiny specks aren't text lines (2+ rows, 5+ cols).
            if count >= 20 && maxx - minx >= 4 && maxy - miny >= 1 {
                boxes.push([minx, miny, maxx + 1, maxy + 1]);
            }
        }
    }
    // Top-to-bottom, then left-to-right — a sane initial order for downstream.
    boxes.sort_by(|a, b| a[1].cmp(&b[1]).then(a[0].cmp(&b[0])));
    boxes
}

/// Rectangular approximation of DB's polygon unclip: offset each side by
/// `area * UNCLIP_RATIO / perimeter`, clamped to the valid region.
fn unclip(b: [usize; 4], sw: usize, sh: usize) -> [usize; 4] {
    let (w, h) = ((b[2] - b[0]) as f32, (b[3] - b[1]) as f32);
    let d = (w * h * UNCLIP_RATIO / (2.0 * (w + h)).max(1.0)).ceil() as usize;
    [
        b[0].saturating_sub(d),
        b[1].saturating_sub(d),
        (b[2] + d).min(sw),
        (b[3] + d).min(sh),
    ]
}

/// CTC greedy decode: argmax per step, collapse repeats, drop blank (class 0).
/// Class `1..=dict.len()` maps to dictionary entries; the trailing extra class
/// is space. Confidence = mean max-softmax over emitting steps.
fn ctc_greedy(logits: &[f32], steps: usize, classes: usize, dict: &[String]) -> (String, f32) {
    let mut text = String::new();
    let mut last = usize::MAX;
    let mut conf_sum = 0.0f32;
    let mut conf_n = 0usize;
    for t in 0..steps {
        let row = &logits[t * classes..(t + 1) * classes];
        let (mut bi, mut bv) = (0usize, f32::MIN);
        for (i, &v) in row.iter().enumerate() {
            if v > bv {
                bv = v;
                bi = i;
            }
        }
        if bi != 0 && bi != last {
            // The PP-OCR rec head already ends in softmax — the argmax value
            // IS the step probability (re-softmaxing 6625 near-zero logits
            // would flatten everything to ~0). Clamp in case a future model
            // exports raw logits.
            conf_sum += bv.clamp(0.0, 1.0);
            conf_n += 1;
            if bi - 1 < dict.len() {
                text.push_str(&dict[bi - 1]);
            } else {
                text.push(' ');
            }
        }
        last = bi;
    }
    let conf = if conf_n == 0 {
        0.0
    } else {
        conf_sum / conf_n as f32
    };
    (text, conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctc_collapses_repeats_and_blanks() {
        let dict: Vec<String> = vec!["a".into(), "b".into()];
        // classes: 0=blank, 1=a, 2=b, 3=space. Steps: a a blank b
        #[rustfmt::skip]
        let logits = [
            0.0, 5.0, 0.0, 0.0,
            0.0, 5.0, 0.0, 0.0,
            5.0, 0.0, 0.0, 0.0,
            0.0, 0.0, 5.0, 0.0,
        ];
        let (text, conf) = ctc_greedy(&logits, 4, 4, &dict);
        assert_eq!(text, "ab");
        assert!(
            conf > 0.9,
            "high-margin logits → high confidence, got {conf}"
        );
    }

    #[test]
    fn ctc_trailing_class_is_space() {
        let dict: Vec<String> = vec!["x".into()];
        // classes: 0=blank, 1=x, 2=space
        let logits = [0.0, 5.0, 0.0, 0.0, 0.0, 5.0, 0.0, 5.0, 0.0];
        let (text, _) = ctc_greedy(&logits, 3, 3, &dict);
        assert_eq!(text, "x x");
    }

    #[test]
    fn component_boxes_finds_separate_bands() {
        // Two horizontal bands in a 16x16 map (stride 16).
        let mut map = vec![0.0f32; 16 * 16];
        for x in 2..14 {
            map[3 * 16 + x] = 0.9;
            map[4 * 16 + x] = 0.9;
            map[10 * 16 + x] = 0.9;
            map[11 * 16 + x] = 0.9;
        }
        let boxes = component_boxes(&map, 16, 16, 16, 0.3);
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0], [2, 3, 14, 5]);
        assert_eq!(boxes[1], [2, 10, 14, 12]);
    }

    #[test]
    fn unclip_expands_and_clamps() {
        let b = unclip([0, 0, 10, 4], 12, 12);
        assert!(b[2] > 10 || b[3] > 4);
        assert!(b[2] <= 12 && b[3] <= 12);
    }

    #[test]
    fn bilinear_resize_preserves_flat_color() {
        let src = vec![100u8; 8 * 8 * 3];
        let out = resize_bilinear(&src, 8, 8, 3, 5);
        assert!(out.iter().all(|&v| v == 100));
    }

    #[test]
    fn sanitize_dims_is_length_preserving() {
        let input = b"xx p2o.DynamicDimension.0 yy p2o.DynamicDimension.12 zz".to_vec();
        let out = sanitize_dims(&input);
        assert_eq!(out.len(), input.len());
        assert!(!out.windows(21).any(|w| w == b"p2o.DynamicDimension."));
    }

    #[test]
    fn parse_yml_char_dict_handles_quotes_and_block_end() {
        // Mirrors a PP-OCRv6 rec inference.yml: quoted, bare, escaped, space,
        // then a following key that must end the block.
        let yml = "PostProcess:\n  name: CTCLabelDecode\n  character_dict:\n  - '!'\n  - '\"'\n  - $\n  - ''''\n  - ' '\n  - 枯\n  use_space_char: true\n";
        let d = parse_yml_char_dict(yml).unwrap();
        assert_eq!(d, ["!", "\"", "$", "'", " ", "枯"]);
    }

    #[test]
    fn rotate_rgb_quarter_turns() {
        // 2x1 image: [A, B] (A=red, B=green).
        let src = [255, 0, 0, 0, 255, 0];
        // 90 CW → 1x2: A on top? src(0,0)→dst(0,0): A top, B bottom.
        let r1 = rotate_rgb(&src, 2, 1, 1);
        assert_eq!(&r1[..3], &[255, 0, 0]);
        assert_eq!(&r1[3..], &[0, 255, 0]);
        // 180 → [B, A].
        let r2 = rotate_rgb(&src, 2, 1, 2);
        assert_eq!(&r2[..3], &[0, 255, 0]);
        // Four single turns compose to identity.
        let back = rotate_rgb(&rotate_rgb(&r1, 1, 2, 1), 2, 1, 2);
        assert_eq!(&back[..], &src[..]);
    }

    #[test]
    fn rotate_box_round_trips() {
        let b = [10, 20, 110, 40]; // in a 200x100 frame
        let r = rotate_box(b, 200, 100, 1); // frame becomes 100x200
        assert_eq!(r, [60, 10, 80, 110]);
        // Inverse turn count restores the original.
        assert_eq!(rotate_box(r, 100, 200, 3), b);
        assert_eq!(rotate_box(rotate_box(b, 200, 100, 2), 200, 100, 2), b);
    }
}

#[cfg(test)]
mod mixed_tests {
    use super::overlaps_existing;
    use docparse_core::ir::BBox;

    #[test]
    fn ocr_overlapping_digital_text_is_dropped() {
        let existing = vec![BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 20.0,
        }];
        // fully inside existing text → dropped
        assert!(overlaps_existing(
            &BBox {
                x0: 10.0,
                y0: 5.0,
                x1: 90.0,
                y1: 15.0
            },
            &existing
        ));
        // elsewhere on the page → kept
        assert!(!overlaps_existing(
            &BBox {
                x0: 0.0,
                y0: 100.0,
                x1: 80.0,
                y1: 120.0
            },
            &existing
        ));
        // grazing overlap (<50%) → kept
        assert!(!overlaps_existing(
            &BBox {
                x0: 90.0,
                y0: 10.0,
                x1: 200.0,
                y1: 30.0
            },
            &existing
        ));
    }
}
