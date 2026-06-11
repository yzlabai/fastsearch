//! ONNX-embedded OCR enhancer — the N3/P4 route: PP-OCRv4 mobile models
//! (RapidOCR's ONNX export) running on `tract`, a pure-Rust inference runtime.
//! No Python, no C++, no subprocess; models are *external files* loaded at
//! runtime, so the core stays model-free and the deterministic path never
//! touches this crate (cost thesis: only quality-flagged pages route here).
//!
//! Pipeline (mirrors RapidOCR's, independently implemented):
//!   page image (extracted XObject pixels, never rasterized)
//!   → det (DBNet, 960×960 padded canvas) → threshold → connected components
//!   → boxes (rect offset ≈ DB unclip) → per-box crop, resize h=48, width
//!   buckets → rec (SVTR-LCNet) → CTC greedy decode + mean-prob confidence
//!   → TextChunks in PDF user space (pixel coords mapped through the image's
//!   placement bbox), `source: "ocr:ppocr-v4"`, confidence < 1.
//!
//! Model files (see docs/plans/n3-real-enhancer.md): ch_PP-OCRv4_det_infer.onnx,
//! ch_PP-OCRv4_rec_infer.onnx, ppocr_keys_v1.txt. paddle2onnx dim names contain
//! dots tract can't parse — sanitized in-memory at load. TODO: orientation
//! (cls) model — upright scans assumed.

pub mod layout;

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

pub struct PpOcrEnhancer {
    det: Runnable,
    rec_bytes: Vec<u8>,
    rec_cache: Mutex<HashMap<usize, Runnable>>,
    dict: Vec<String>,
}

impl PpOcrEnhancer {
    /// Load models from a directory. Fails with a traceable error when files
    /// are missing — callers decide whether OCR is optional.
    pub fn new(model_dir: &Path) -> Result<Self> {
        // Accept any PP-OCR generation: the v4 file names, or generic ones
        // (det*.onnx / rec*.onnx / *dict*.txt | ppocr_keys*) so a PP-OCRv5
        // model set drops in without renames.
        let det_path = find_file(model_dir, &["ch_PP-OCRv4_det_infer.onnx"], "det", ".onnx")?;
        let rec_path = find_file(model_dir, &["ch_PP-OCRv4_rec_infer.onnx"], "rec", ".onnx")?;
        let dict_path = find_file(model_dir, &["ppocr_keys_v1.txt"], "dict", ".txt")?;
        let det_bytes = sanitize_dims(
            &std::fs::read(&det_path)
                .with_context(|| format!("det model {}", det_path.display()))?,
        );
        let rec_bytes = sanitize_dims(
            &std::fs::read(&rec_path)
                .with_context(|| format!("rec model {}", rec_path.display()))?,
        );
        let dict: Vec<String> = std::fs::read_to_string(&dict_path)
            .with_context(|| format!("dictionary {}", dict_path.display()))?
            .lines()
            .map(str::to_owned)
            .collect();
        anyhow::ensure!(
            dict.len() > 6000,
            "dictionary looks truncated: {} lines",
            dict.len()
        );

        let det = tract_onnx::onnx()
            .model_for_read(&mut &det_bytes[..])?
            .with_input_fact(0, f32::fact([1, 3, DET_SIDE, DET_SIDE]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self {
            det,
            rec_bytes,
            rec_cache: Mutex::new(HashMap::new()),
            dict,
        })
    }

    /// OCR one RGB image; returns (text, bbox-in-pixels, confidence) per line.
    /// Pixel bboxes are (x0, y0_top, x1, y1_bottom) in image coordinates.
    fn ocr_rgb(&self, rgb: &[u8], w: usize, h: usize) -> Result<Vec<(String, [usize; 4], f32)>> {
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

        // ---- rec each box (top-to-bottom; layout re-orders downstream) ----
        let mut results = Vec::new();
        for b in boxes {
            let [bx0, by0, bx1, by1] = unclip(b, sw, sh);
            // Back to original pixel coords.
            let inv = 1.0 / ratio;
            let (ox0, oy0) = ((bx0 as f32 * inv) as usize, (by0 as f32 * inv) as usize);
            let (ox1, oy1) = (
                ((bx1 as f32 * inv) as usize).min(w),
                ((by1 as f32 * inv) as usize).min(h),
            );
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
                tract_onnx::onnx()
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
    /// backend attached pixels to). Returns the original elements plus one
    /// `TextChunk` per recognized line, or `None` when there's nothing usable.
    fn enhance_page(&self, page: &Page) -> Option<Vec<Element>> {
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
        // Surface inference errors instead of silently declining (they mean a
        // broken model/setup, not a hard page) — then decline so the
        // deterministic result still stands.
        let lines = match self.ocr_rgb(&rgb, w, h) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "ppocr-onnx: inference failed on page {}: {e:#}",
                    page.number
                );
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
        let mut elements = page.elements.clone();
        let (bw, bh) = (img.bbox.x1 - img.bbox.x0, img.bbox.y1 - img.bbox.y0);
        for (text, [px0, py0, px1, py1], conf) in lines {
            let x0 = img.bbox.x0 + px0 as f32 / w as f32 * bw;
            let x1 = img.bbox.x0 + px1 as f32 / w as f32 * bw;
            // Pixel y runs top-down; PDF y runs bottom-up.
            let y1 = img.bbox.y1 - py0 as f32 / h as f32 * bh;
            let y0 = img.bbox.y1 - py1 as f32 / h as f32 * bh;
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
                source: Some("ocr:ppocr-v4".into()),
                group: None,
                tag: None,
            }));
        }
        Some(elements)
    }
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

/// Resolve a model file: exact known names first, then any file whose name
/// contains `needle` with the given extension (alphabetically first match).
fn find_file(dir: &Path, exact: &[&str], needle: &str, ext: &str) -> Result<std::path::PathBuf> {
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
fn resize_bilinear(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
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
