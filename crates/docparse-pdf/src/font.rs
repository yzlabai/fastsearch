//! Per-font decoding model, assembled from a page's `/Resources /Font` dict.
//!
//! For each font we resolve what veraPDF's `PDFont`/`PDSimpleFont`/
//! `PDType0Font` provide for text extraction:
//! - a `ToUnicode` [`CMap`] (code → text) and its codespaces (byte splitting),
//! - glyph widths (simple `Widths`+`FirstChar`, or CID `W`+`DW`),
//! so the interpreter can turn a show-string into real text and advance the
//! pen by true glyph widths instead of a flat estimate.

use crate::cmap::CMap;
use lopdf::{Dictionary, Document as PdfDocument, Object, ObjectId};
use std::collections::HashMap;

/// Glyph width source, in 1/1000 em (PDF glyph-space units).
enum Widths {
    /// Simple font: `Widths[code - first_char]`.
    Simple { first: i64, w: Vec<f64> },
    /// CID font: code → width, with a default for unmapped CIDs.
    Cid(HashMap<u32, f64>),
}

/// Everything needed to decode show-strings for one font.
pub struct FontInfo {
    is_type0: bool,
    to_unicode: Option<CMap>,
    widths: Widths,
    default_width: f64,
}

/// Default advance when a glyph width is unknown (0.5 em, in 1/1000 units).
const FALLBACK_WIDTH: f64 = 500.0;

impl FontInfo {
    /// Decode a show-string into (text, total advance in 1/1000 em).
    pub fn decode(&self, bytes: &[u8]) -> (String, f64) {
        let mut text = String::new();
        let mut advance = 0.0;
        let mut pos = 0;
        while pos < bytes.len() {
            let (code, len) = self.next_code(bytes, pos);
            pos += len;

            match &self.to_unicode {
                Some(cm) => {
                    if let Some(u) = cm.unicode(code) {
                        text.push_str(&u);
                    }
                }
                // Simple font without ToUnicode: best-effort Latin-1 (the prior
                // behavior, which already worked for standard-encoded fonts).
                None if !self.is_type0 => {
                    if (0x20..=0xFF).contains(&code) {
                        text.push(code as u8 as char);
                    }
                }
                // Composite font without ToUnicode: code is a glyph id we can't
                // map. Leave text empty but still advance the pen.
                None => {}
            }

            advance += self.width(code).unwrap_or(FALLBACK_WIDTH);
        }
        (text, advance)
    }

    /// Split off the next character code (veraPDF codespace logic, or a fixed
    /// width when no ToUnicode codespaces are declared).
    fn next_code(&self, data: &[u8], pos: usize) -> (u32, usize) {
        if let Some(cm) = &self.to_unicode {
            if !cm.codespaces.is_empty() {
                return cm.next_code(data, pos);
            }
        }
        let len = if self.is_type0 { 2 } else { 1 }.min(data.len() - pos).max(1);
        let mut code = 0u32;
        for &b in &data[pos..pos + len] {
            code = (code << 8) | b as u32;
        }
        (code, len)
    }

    fn width(&self, code: u32) -> Option<f64> {
        match &self.widths {
            Widths::Simple { first, w } => {
                let idx = code as i64 - first;
                if idx >= 0 && (idx as usize) < w.len() {
                    Some(w[idx as usize])
                } else {
                    None
                }
            }
            Widths::Cid(map) => Some(map.get(&code).copied().unwrap_or(self.default_width)),
        }
    }
}

/// Build decoders for every font in a page's resources (with Pages-tree
/// inheritance for both `/Resources` and the font dict).
pub fn build_page_fonts(doc: &PdfDocument, page_id: ObjectId) -> HashMap<String, FontInfo> {
    let mut out = HashMap::new();
    let Some(resources) = resolve_resources(doc, page_id) else {
        return out;
    };
    let Some(Object::Dictionary(fonts)) = resources.get(b"Font").ok().and_then(|o| deref(doc, o))
    else {
        return out;
    };
    for (name, val) in fonts.iter() {
        if let Some(Object::Dictionary(fd)) = deref(doc, val) {
            out.insert(String::from_utf8_lossy(name).into_owned(), build_font(doc, fd));
        }
    }
    out
}

fn build_font(doc: &PdfDocument, fd: &Dictionary) -> FontInfo {
    let is_type0 = name_of(fd, b"Subtype").as_deref() == Some("Type0");

    let to_unicode = fd
        .get(b"ToUnicode")
        .ok()
        .and_then(|o| stream_bytes(doc, o))
        .map(|b| CMap::parse(&b));

    let (widths, default_width) = if is_type0 {
        cid_widths(doc, fd)
    } else {
        let first = int_of(fd, b"FirstChar").unwrap_or(0);
        let w = array_of(doc, fd, b"Widths")
            .map(|a| a.iter().filter_map(num).collect())
            .unwrap_or_default();
        (Widths::Simple { first, w }, 0.0)
    };

    FontInfo {
        is_type0,
        to_unicode,
        widths,
        default_width,
    }
}

/// Extract `W`/`DW` from the descendant CID font. The `W` array is a sequence
/// of either `c [w1 w2 ...]` or `c_first c_last w`.
fn cid_widths(doc: &PdfDocument, type0: &Dictionary) -> (Widths, f64) {
    let mut map = HashMap::new();
    let mut dw = 1000.0;

    if let Some(Object::Array(descendants)) =
        type0.get(b"DescendantFonts").ok().and_then(|o| deref(doc, o))
    {
        if let Some(Object::Dictionary(cid_font)) = descendants.first().and_then(|o| deref(doc, o)) {
            dw = int_of(cid_font, b"DW").map(|v| v as f64).unwrap_or(1000.0);
            if let Some(w) = array_of(doc, cid_font, b"W") {
                parse_w_array(doc, w, &mut map);
            }
        }
    }
    (Widths::Cid(map), dw)
}

fn parse_w_array(doc: &PdfDocument, w: &[Object], map: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < w.len() {
        let Some(cstart) = num(&w[i]) else {
            i += 1;
            continue;
        };
        match w.get(i + 1).and_then(|o| deref(doc, o)) {
            // c [w1 w2 ...]
            Some(Object::Array(ws)) => {
                for (k, wv) in ws.iter().enumerate() {
                    if let Some(width) = num(wv) {
                        map.insert(cstart as u32 + k as u32, width);
                    }
                }
                i += 2;
            }
            // c_first c_last w
            _ => {
                let clast = w.get(i + 1).and_then(num);
                let width = w.get(i + 2).and_then(num);
                if let (Some(cl), Some(width)) = (clast, width) {
                    for cc in cstart as u32..=cl as u32 {
                        map.insert(cc, width);
                    }
                }
                i += 3;
            }
        }
    }
}

// ---- resources / dictionary helpers -------------------------------------

fn resolve_resources<'a>(doc: &'a PdfDocument, page_id: ObjectId) -> Option<&'a Dictionary> {
    let mut id = page_id;
    for _ in 0..16 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Some(Object::Dictionary(res)) = dict.get(b"Resources").ok().and_then(|o| deref(doc, o))
        {
            return Some(res);
        }
        match dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok()) {
            Some(parent) => id = parent,
            None => return None,
        }
    }
    None
}

/// Resolve a (possibly indirect) object reference.
fn deref<'a>(doc: &'a PdfDocument, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

fn stream_bytes(doc: &PdfDocument, obj: &Object) -> Option<Vec<u8>> {
    let stream = deref(doc, obj)?.as_stream().ok()?;
    stream
        .decompressed_content()
        .ok()
        .or_else(|| Some(stream.content.clone()))
}

fn array_of<'a>(doc: &'a PdfDocument, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Vec<Object>> {
    match dict.get(key).ok().and_then(|o| deref(doc, o)) {
        Some(Object::Array(a)) => Some(a),
        _ => None,
    }
}

fn name_of(dict: &Dictionary, key: &[u8]) -> Option<String> {
    match dict.get(key) {
        Ok(Object::Name(n)) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

fn int_of(dict: &Dictionary, key: &[u8]) -> Option<i64> {
    num(dict.get(key).ok()?).map(|f| f as i64)
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}
