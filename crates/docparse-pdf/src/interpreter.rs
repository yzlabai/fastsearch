//! A minimal PDF content-stream interpreter.
//!
//! lopdf gives us the *parsed operator list* (`Content::decode`); this module
//! is the part opendataloader-pdf delegates to veraPDF: walk the text-showing
//! operators, track the graphics/text matrices, and emit positioned
//! [`TextChunk`]s. It deliberately does NOT rasterize.
//!
//! Operators handled: q Q cm  BT ET  Tf TL Td TD Tm T*  Tj ' TJ.
//!
//! Known approximations (tracked as TODOs):
//! - Glyph widths are estimated (0.5 em/char) instead of read from font metrics,
//!   so x-extents and inter-chunk advances are approximate.
//! - Text bytes are decoded best-effort as Latin-1; CID fonts and ToUnicode
//!   CMaps are not yet honored.
//! - Char/word spacing (Tc/Tw) and horizontal scaling (Tz) are ignored.

use crate::font::FontInfo;
use crate::matrix::Matrix;
use docparse_core::ir::{BBox, Element, Page, TextChunk};
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
}

#[derive(Clone)]
struct TextState {
    tm: Matrix,
    tlm: Matrix,
    font_size: f64,
    leading: f64,
    font: Option<String>,
}

impl TextState {
    fn new() -> Self {
        Self {
            tm: Matrix::identity(),
            tlm: Matrix::identity(),
            font_size: 0.0,
            leading: 0.0,
            font: None,
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
            "Tf" => {
                if ops.len() >= 2 {
                    ts.font = name_of(&ops[0]);
                    ts.font_size = num(&ops[1]).unwrap_or(ts.font_size);
                }
            }
            "TL" => {
                if let Some(v) = num0(ops, 0) {
                    ts.leading = v;
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
                    show_text(bytes, &mut ts, &ctm, &mut elements, input.number, &input.fonts);
                }
            }
            "'" => {
                ts.tlm = Matrix::translate(0.0, -ts.leading).mul(&ts.tlm);
                ts.tm = ts.tlm;
                if let Some(Object::String(bytes, _)) = ops.first() {
                    show_text(bytes, &mut ts, &ctm, &mut elements, input.number, &input.fonts);
                }
            }
            "TJ" => {
                if let Some(Object::Array(arr)) = ops.first() {
                    for el in arr {
                        match el {
                            Object::String(bytes, _) => {
                                show_text(bytes, &mut ts, &ctm, &mut elements, input.number, &input.fonts)
                            }
                            _ => {
                                if let Some(adj) = num(el) {
                                    // Negative adjustment moves the pen forward.
                                    let dx = -adj / 1000.0 * ts.font_size;
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

    Page {
        number: input.number,
        width: input.width,
        height: input.height,
        elements,
    }
}

/// Fallback glyph advance (em fraction) when no font decoder is available.
const FALLBACK_ADVANCE_EM: f64 = 0.5;

fn show_text(
    bytes: &[u8],
    ts: &mut TextState,
    ctm: &Matrix,
    out: &mut Vec<Element>,
    page: usize,
    fonts: &HashMap<String, FontInfo>,
) {
    let font = ts.font.as_ref().and_then(|name| fonts.get(name));

    // Decode text and total advance (in 1/1000 em) via the font, or fall back
    // to Latin-1 + a flat estimate when the font is unknown.
    let (text, advance_em_thousandths) = match font {
        Some(fi) => fi.decode(bytes),
        None => {
            let t = decode_bytes(bytes);
            let est = t.chars().count() as f64 * FALLBACK_ADVANCE_EM * 1000.0;
            (t, est)
        }
    };

    let trm = ts.tm.mul(ctm);
    let w_text = advance_em_thousandths / 1000.0 * ts.font_size;
    let (x_start, y_base) = trm.apply(0.0, 0.0);
    let (x_end, _) = trm.apply(w_text, 0.0);
    let height = ts.font_size * trm.y_scale();

    if !text.trim().is_empty() {
        let x0 = x_start.min(x_end) as f32;
        let x1 = x_start.max(x_end) as f32;
        let y0 = y_base as f32;
        out.push(Element::Text(TextChunk {
            text,
            bbox: BBox {
                x0,
                y0,
                x1,
                y1: y0 + height as f32,
            },
            font_size: height as f32,
            font: ts.font.clone(),
            page,
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
