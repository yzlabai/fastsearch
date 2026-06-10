//! A minimal PDF content-stream interpreter.
//!
//! lopdf gives us the *parsed operator list* (`Content::decode`); this module
//! is the part opendataloader-pdf delegates to veraPDF: walk the text-showing
//! operators, track the graphics/text matrices, and emit positioned
//! [`TextChunk`]s. It deliberately does NOT rasterize.
//!
//! Operators handled: q Q cm  BT ET  Tf TL Tc Tw Tz Td TD Tm T*  Tj ' TJ.
//!
//! Text decoding and glyph widths come from [`crate::font`] (ToUnicode/AGL and
//! real font metrics); `Tc`/`Tw`/`Tz` are honored in the displacement formula.
//!
//! Known approximations (tracked as TODOs):
//! - Text rise (`Ts`) is ignored; an unknown font still falls back to
//!   Latin-1 + 0.5 em/char.
//!
//! Security pre-check (N5a, ref ODL hidden-text filtering): text that a human
//! can't see — render mode `Tr 3`/`Tr 7`, fully off-page bbox, or sub-readable
//! font size — is emitted with `hidden: true` so the core excludes it from
//! rendered outputs while keeping it auditable in the IR. TODO (flagged, not
//! detected): same-color-as-background text, text occluded by images.

use crate::font::FontInfo;
use crate::images::XImage;
use crate::matrix::Matrix;
use docparse_core::ir::{BBox, Element, ImageChunk, ImageKind, Page, TextChunk};
use docparse_core::table::{detect_borderless_tables, detect_ruled_tables, detect_tables, Segment};
use docparse_core::table_cluster::detect_cluster_tables;
use lopdf::content::Content;
use lopdf::Object;
use std::collections::HashMap;

/// Everything needed to interpret one page, owned so interpretation can run on
/// a worker thread without touching the shared `lopdf::Document`.
pub struct PageInput {
    pub number: usize,
    pub width: f32,
    pub height: f32,
    pub content: Vec<u8>,
    /// Font decoders keyed by resource name (e.g. "F1"), resolved up-front.
    pub fonts: HashMap<String, FontInfo>,
    /// Image XObjects keyed by resource name (for `Do`), streams undecoded.
    pub images: HashMap<String, XImage>,
}

#[derive(Clone)]
struct TextState {
    tm: Matrix,
    tlm: Matrix,
    font_size: f64,
    leading: f64,
    font: Option<String>,
    /// `Tc` character spacing, unscaled text-space units.
    char_spacing: f64,
    /// `Tw` word spacing, unscaled text-space units.
    word_spacing: f64,
    /// `Tz` horizontal scaling as a factor (100% → 1.0).
    h_scale: f64,
    /// `Tr` text render mode. 3 = invisible, 7 = clip-only — both unseen.
    render_mode: i64,
}

impl TextState {
    fn new() -> Self {
        Self {
            tm: Matrix::identity(),
            tlm: Matrix::identity(),
            font_size: 0.0,
            leading: 0.0,
            font: None,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scale: 1.0,
            render_mode: 0,
        }
    }
}

/// Interpret a page's content stream into positioned text chunks.
pub fn interpret(input: &PageInput) -> Page {
    let mut elements: Vec<Element> = Vec::new();

    let content = match Content::decode(&input.content) {
        Ok(c) => c,
        Err(_) => {
            // Unparseable/empty stream (e.g. scanned page) — return an empty page.
            return Page {
                number: input.number,
                width: input.width,
                height: input.height,
                elements,
            };
        }
    };

    let mut ctm_stack: Vec<Matrix> = Vec::new();
    let mut ctm = Matrix::identity();
    let mut ts = TextState::new();

    // Vector-path state for ruling-line (table border) extraction.
    let mut cur_pt: Option<(f64, f64)> = None; // current point (user space)
    let mut sub_start: Option<(f64, f64)> = None; // subpath start (for closepath)
    let mut path: Vec<Segment> = Vec::new(); // segments of the path being built
    let mut segments: Vec<Segment> = Vec::new(); // painted (stroked/filled) segments

    for op in &content.operations {
        let ops = &op.operands;
        match op.operator.as_str() {
            "q" => ctm_stack.push(ctm),
            "Q" => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
            }
            "cm" => {
                if let Some(m) = matrix_from(ops) {
                    ctm = m.mul(&ctm);
                }
            }
            "BT" => {
                ts.tm = Matrix::identity();
                ts.tlm = Matrix::identity();
            }
            "Tf" if ops.len() >= 2 => {
                ts.font = name_of(&ops[0]);
                ts.font_size = num(&ops[1]).unwrap_or(ts.font_size);
            }
            "TL" => {
                if let Some(v) = num0(ops, 0) {
                    ts.leading = v;
                }
            }
            "Tc" => {
                if let Some(v) = num0(ops, 0) {
                    ts.char_spacing = v;
                }
            }
            "Tw" => {
                if let Some(v) = num0(ops, 0) {
                    ts.word_spacing = v;
                }
            }
            "Tz" => {
                if let Some(v) = num0(ops, 0) {
                    ts.h_scale = v / 100.0;
                }
            }
            "Tr" => {
                if let Some(v) = num0(ops, 0) {
                    ts.render_mode = v as i64;
                }
            }
            "Td" => {
                if let (Some(tx), Some(ty)) = (num0(ops, 0), num0(ops, 1)) {
                    ts.tlm = Matrix::translate(tx, ty).mul(&ts.tlm);
                    ts.tm = ts.tlm;
                }
            }
            "TD" => {
                if let (Some(tx), Some(ty)) = (num0(ops, 0), num0(ops, 1)) {
                    ts.leading = -ty;
                    ts.tlm = Matrix::translate(tx, ty).mul(&ts.tlm);
                    ts.tm = ts.tlm;
                }
            }
            "Tm" => {
                if let Some(m) = matrix_from(ops) {
                    ts.tlm = m;
                    ts.tm = m;
                }
            }
            "T*" => {
                ts.tlm = Matrix::translate(0.0, -ts.leading).mul(&ts.tlm);
                ts.tm = ts.tlm;
            }
            "Tj" => {
                if let Some(Object::String(bytes, _)) = ops.first() {
                    show_text(
                        bytes,
                        &mut ts,
                        &ctm,
                        &mut elements,
                        input.number,
                        (input.width, input.height),
                        &input.fonts,
                    );
                }
            }
            "'" => {
                ts.tlm = Matrix::translate(0.0, -ts.leading).mul(&ts.tlm);
                ts.tm = ts.tlm;
                if let Some(Object::String(bytes, _)) = ops.first() {
                    show_text(
                        bytes,
                        &mut ts,
                        &ctm,
                        &mut elements,
                        input.number,
                        (input.width, input.height),
                        &input.fonts,
                    );
                }
            }
            // ---- path construction (for table ruling lines) ----
            "m" => {
                if let (Some(x), Some(y)) = (num0(ops, 0), num0(ops, 1)) {
                    let p = ctm.apply(x, y);
                    cur_pt = Some(p);
                    sub_start = Some(p);
                }
            }
            "l" => {
                if let (Some(x), Some(y)) = (num0(ops, 0), num0(ops, 1)) {
                    let p = ctm.apply(x, y);
                    if let Some(a) = cur_pt {
                        path.push(seg(a, p));
                    }
                    cur_pt = Some(p);
                }
            }
            "re" => {
                if let (Some(x), Some(y), Some(w), Some(h)) =
                    (num0(ops, 0), num0(ops, 1), num0(ops, 2), num0(ops, 3))
                {
                    let p00 = ctm.apply(x, y);
                    let p10 = ctm.apply(x + w, y);
                    let p11 = ctm.apply(x + w, y + h);
                    let p01 = ctm.apply(x, y + h);
                    path.push(seg(p00, p10));
                    path.push(seg(p10, p11));
                    path.push(seg(p11, p01));
                    path.push(seg(p01, p00));
                    cur_pt = Some(p00);
                    sub_start = Some(p00);
                }
            }
            // Curves: not table borders — just advance the current point.
            "c" => cur_pt = num0(ops, 4).zip(num0(ops, 5)).map(|(x, y)| ctm.apply(x, y)),
            "v" | "y" => cur_pt = num0(ops, 2).zip(num0(ops, 3)).map(|(x, y)| ctm.apply(x, y)),
            "h" => {
                if let (Some(a), Some(s)) = (cur_pt, sub_start) {
                    path.push(seg(a, s));
                    cur_pt = Some(s);
                }
            }
            // Painting ops keep the path → flush as real ruling lines.
            "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => {
                segments.append(&mut path);
                cur_pt = None;
                sub_start = None;
            }
            // `n` ends the path without painting (e.g. after a clip) → discard.
            "n" => {
                path.clear();
                cur_pt = None;
                sub_start = None;
            }
            // XObject placement. Images are recorded with their placement bbox
            // (CTM applied to the unit square); pixels are materialized only
            // for page-covering images — the scanned-page shape the OCR
            // enhancer consumes (N3) — so figure-heavy digital docs pay ~0.
            "Do" => {
                if let Some(img) = name_of0(ops).and_then(|n| input.images.get(&n)) {
                    let corners = [
                        ctm.apply(0.0, 0.0),
                        ctm.apply(1.0, 0.0),
                        ctm.apply(0.0, 1.0),
                        ctm.apply(1.0, 1.0),
                    ];
                    let x0 = corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min) as f32;
                    let x1 = corners
                        .iter()
                        .map(|c| c.0)
                        .fold(f64::NEG_INFINITY, f64::max) as f32;
                    let y0 = corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min) as f32;
                    let y1 = corners
                        .iter()
                        .map(|c| c.1)
                        .fold(f64::NEG_INFINITY, f64::max) as f32;
                    let coverage = ((x1 - x0) * (y1 - y0)) / (input.width * input.height).max(1.0);
                    let (kind, data) = if coverage >= SCAN_COVERAGE_MIN {
                        img.decode()
                    } else {
                        (ImageKind::None, Vec::new())
                    };
                    elements.push(Element::Image(ImageChunk {
                        bbox: BBox { x0, y0, x1, y1 },
                        page: input.number,
                        width_px: img.width,
                        height_px: img.height,
                        kind,
                        data,
                    }));
                }
            }
            "TJ" => {
                if let Some(Object::Array(arr)) = ops.first() {
                    for el in arr {
                        match el {
                            Object::String(bytes, _) => show_text(
                                bytes,
                                &mut ts,
                                &ctm,
                                &mut elements,
                                input.number,
                                (input.width, input.height),
                                &input.fonts,
                            ),
                            _ => {
                                if let Some(adj) = num(el) {
                                    // Negative adjustment moves the pen forward;
                                    // horizontal scaling (Tz) applies.
                                    let dx = -adj / 1000.0 * ts.font_size * ts.h_scale;
                                    ts.tm = Matrix::translate(dx, 0.0).mul(&ts.tm);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Semantic layer: detect bordered tables from ruling lines + text, then
    // append them as elements (the output layer skips text inside table bboxes).
    let text_refs: Vec<&TextChunk> = elements
        .iter()
        .filter_map(|e| match e {
            Element::Text(t) => Some(t),
            _ => None,
        })
        .collect();
    let bordered = detect_tables(&text_refs, &segments, input.number);
    let mut excl: Vec<BBox> = bordered.iter().map(|t| t.bbox).collect();
    // Ruled (booktabs) tables bounded by wide horizontal rules — high-confidence.
    let ruled = detect_ruled_tables(&text_refs, &segments, &excl, input.number);
    excl.extend(ruled.iter().map(|t| t.bbox));
    // Cluster (header-anchored) tables — highest coverage; high-confidence
    // clean-grid path. Runs before the looser borderless fallback.
    let cluster = detect_cluster_tables(&text_refs, &excl, input.number);
    excl.extend(cluster.iter().map(|t| t.bbox));
    // Borderless (alignment-based) tables on text not in any detected table.
    let borderless = detect_borderless_tables(&text_refs, &excl);
    drop(text_refs);
    elements.extend(
        bordered
            .into_iter()
            .chain(ruled)
            .chain(cluster)
            .chain(borderless)
            .map(Element::Table),
    );

    Page {
        number: input.number,
        width: input.width,
        height: input.height,
        elements,
    }
}

/// Build an axis-classifiable [`Segment`] from two user-space points.
fn seg(a: (f64, f64), b: (f64, f64)) -> Segment {
    Segment {
        x0: a.0 as f32,
        y0: a.1 as f32,
        x1: b.0 as f32,
        y1: b.1 as f32,
    }
}

/// Fallback glyph advance (em fraction) when no font decoder is available.
const FALLBACK_ADVANCE_EM: f64 = 0.5;

/// An image whose placement covers at least this fraction of the page is a
/// scan candidate: its pixels are decoded and attached for the OCR enhancer.
/// Smaller figures stay position-only, bounding memory on digital documents.
const SCAN_COVERAGE_MIN: f32 = 0.5;

/// Below this effective glyph height (pt) text is unreadable to a human and
/// treated as hidden. Normal subscripts run 5–7pt; 1pt is far under legibility.
const TINY_FONT_PT: f32 = 1.0;

fn show_text(
    bytes: &[u8],
    ts: &mut TextState,
    ctm: &Matrix,
    out: &mut Vec<Element>,
    page: usize,
    page_size: (f32, f32),
    fonts: &HashMap<String, FontInfo>,
) {
    let font = ts.font.as_ref().and_then(|name| fonts.get(name));

    // Decode text and pen-advance metrics via the font, or fall back to
    // Latin-1 + a flat estimate when the font is unknown.
    let (text, glyph_advance, glyphs, spaces) = match font {
        Some(fi) => {
            let d = fi.decode(bytes);
            (d.text, d.advance, d.glyphs, d.spaces)
        }
        None => {
            let t = decode_bytes(bytes);
            let n = t.chars().count() as u32;
            (t, n as f64 * FALLBACK_ADVANCE_EM * 1000.0, n, 0)
        }
    };

    let trm = ts.tm.mul(ctm);
    // PDF text displacement: tx = (Σwidth·Tfs + Tc·glyphs + Tw·spaces) · Th.
    let w_text = (glyph_advance / 1000.0 * ts.font_size
        + ts.char_spacing * glyphs as f64
        + ts.word_spacing * spaces as f64)
        * ts.h_scale;
    // Axis-aligned bbox from the 4 transformed corners of the text rectangle
    // [0,w_text] × [0,font_size] — correct under rotation (rotated stamps would
    // otherwise collapse to zero width).
    let fs = ts.font_size;
    let corners = [
        trm.apply(0.0, 0.0),
        trm.apply(w_text, 0.0),
        trm.apply(0.0, fs),
        trm.apply(w_text, fs),
    ];
    let height = ts.font_size * trm.y_scale();

    if !text.trim().is_empty() {
        let x0 = corners.iter().map(|c| c.0).fold(f64::INFINITY, f64::min) as f32;
        let x1 = corners
            .iter()
            .map(|c| c.0)
            .fold(f64::NEG_INFINITY, f64::max) as f32;
        let y0 = corners.iter().map(|c| c.1).fold(f64::INFINITY, f64::min) as f32;
        let y1 = corners
            .iter()
            .map(|c| c.1)
            .fold(f64::NEG_INFINITY, f64::max) as f32;
        // Hidden-text classification (N5a): invisible render mode, fully
        // off-page, or sub-readable size — flagged, not dropped.
        let (pw, ph) = page_size;
        let off_page = x1 < 0.0 || y1 < 0.0 || x0 > pw || y0 > ph;
        let hidden = matches!(ts.render_mode, 3 | 7) || off_page || (height as f32) < TINY_FONT_PT;
        out.push(Element::Text(TextChunk {
            text,
            bbox: BBox { x0, y0, x1, y1 },
            font_size: height as f32,
            // Prefer the PostScript name (meaningful to layout: bold/mono
            // detection); fall back to the resource name.
            font: font
                .and_then(|f| f.base_font().map(str::to_owned))
                .or_else(|| ts.font.clone()),
            page,
            confidence: 1.0,
            bold: font.map(|f| f.is_bold()).unwrap_or(false),
            hidden,
            source: None,
            group: None,
        }));
    }

    // Advance the pen for the next show operation.
    ts.tm = Matrix::translate(w_text, 0.0).mul(&ts.tm);
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn num0(ops: &[Object], i: usize) -> Option<f64> {
    ops.get(i).and_then(num)
}

fn name_of(o: &Object) -> Option<String> {
    match o {
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

fn name_of0(ops: &[Object]) -> Option<String> {
    ops.first().and_then(name_of)
}

fn matrix_from(ops: &[Object]) -> Option<Matrix> {
    if ops.len() < 6 {
        return None;
    }
    let v: Vec<f64> = ops.iter().take(6).map(|o| num(o).unwrap_or(0.0)).collect();
    Some(Matrix {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    })
}

/// Best-effort byte→text decode. Drops control bytes; keeps printable Latin-1.
/// TODO: honor font Encoding / ToUnicode CMaps and multi-byte CID strings.
fn decode_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter(|&&b| b >= 0x20)
        .map(|&b| b as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(content: &str) -> Page {
        interpret(&PageInput {
            number: 1,
            width: 612.0,
            height: 792.0,
            content: content.as_bytes().to_vec(),
            fonts: HashMap::new(),
            images: HashMap::new(),
        })
    }

    fn texts(page: &Page) -> Vec<(&str, bool)> {
        page.elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.as_str(), t.hidden)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn invisible_render_mode_is_hidden_and_resets() {
        // Tr 3 hides; Tr 0 restores. The hidden chunk must still be emitted.
        let p = page_with("BT /F1 12 Tf 100 700 Td 3 Tr (secret) Tj 0 Tr (visible) Tj ET");
        assert_eq!(texts(&p), vec![("secret", true), ("visible", false)]);
        // And the visible-only accessor drops it.
        let visible: Vec<&str> = p.text_chunks().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(visible, vec!["visible"]);
    }

    #[test]
    fn clip_only_render_mode_is_hidden() {
        let p = page_with("BT /F1 12 Tf 100 700 Td 7 Tr (clipped) Tj ET");
        assert_eq!(texts(&p), vec![("clipped", true)]);
    }

    #[test]
    fn off_page_text_is_hidden() {
        // Positioned far left of the media box — invisible to a reader.
        let p = page_with("BT /F1 12 Tf -500 700 Td (offpage) Tj 600 0 Td (onpage) Tj ET");
        assert_eq!(texts(&p), vec![("offpage", true), ("onpage", false)]);
    }

    #[test]
    fn tiny_font_text_is_hidden() {
        let p = page_with("BT /F1 0.5 Tf 100 700 Td (micro) Tj /F1 5 Tf (subscript) Tj ET");
        assert_eq!(texts(&p), vec![("micro", true), ("subscript", false)]);
    }
}
