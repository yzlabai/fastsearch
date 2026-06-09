//! Per-font decoding model, assembled from a page's `/Resources /Font` dict.
//!
//! For each font we resolve what veraPDF's `PDFont`/`PDSimpleFont`/
//! `PDType0Font` provide for text extraction:
//! - a `ToUnicode` [`CMap`] (code → text) and its codespaces (byte splitting),
//! - glyph widths (simple `Widths`+`FirstChar`, or CID `W`+`DW`),
//! - for simple fonts without `ToUnicode`, a base `/Encoding` + `/Differences`
//!   (code → glyph name → Unicode via the AGL) and standard-14 AFM widths.
//!
//! So the interpreter can turn a show-string into real text and advance the
//! pen by true glyph widths instead of a flat estimate.

use crate::cmap::CMap;
use crate::encoding::{self, Diff};
use crate::stdmetrics;
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
    /// Simple-font `code -> glyph name` (base encoding + `/Differences`),
    /// used both to decode text (no ToUnicode) and to look up AFM widths.
    encoding: Option<Vec<Option<String>>>,
    /// Standard-14 width table (glyph name -> 1/1000 em), used only when the
    /// font has no embedded `/Widths`.
    afm_widths: Option<&'static HashMap<String, f64>>,
    /// Whether this font is bold (from the BaseFont name or descriptor flags).
    bold: bool,
}

/// Default advance when a glyph width is unknown (0.5 em, in 1/1000 units).
const FALLBACK_WIDTH: f64 = 500.0;

/// Result of decoding one show-string: the text plus the metrics the
/// interpreter needs to advance the pen with `Tc`/`Tw` spacing.
pub struct Decoded {
    pub text: String,
    /// Sum of glyph widths, in 1/1000 em.
    pub advance: f64,
    /// Number of glyphs (for `Tc` char spacing).
    pub glyphs: u32,
    /// Number of single-byte code-32 glyphs (for `Tw` word spacing).
    pub spaces: u32,
}

impl FontInfo {
    /// Decode a show-string into text and pen-advance metrics.
    pub fn decode(&self, bytes: &[u8]) -> Decoded {
        let mut text = String::new();
        let mut advance = 0.0;
        let mut glyphs = 0u32;
        let mut spaces = 0u32;
        let mut pos = 0;
        while pos < bytes.len() {
            let (code, len) = self.next_code(bytes, pos);
            pos += len;
            glyphs += 1;
            // Tw word spacing applies only to a single-byte code 32.
            if code == 32 && len == 1 {
                spaces += 1;
            }

            match &self.to_unicode {
                // ToUnicode is authoritative when present.
                Some(cm) => {
                    if let Some(u) = cm.unicode(code) {
                        text.push_str(&u);
                    }
                }
                // Simple font without ToUnicode: resolve code -> glyph name
                // (base encoding + /Differences) -> Unicode via the AGL.
                None if !self.is_type0 => {
                    if let Some(name) = self.glyph_name(code) {
                        if let Some(u) = encoding::glyph_to_unicode(name) {
                            text.push_str(&u);
                        }
                    } else if (0x20..=0xFF).contains(&code) {
                        // No encoding entry: last-resort Latin-1.
                        text.push(code as u8 as char);
                    }
                }
                // Composite font without ToUnicode: code is a glyph id we can't
                // map. Leave text empty but still advance the pen.
                None => {}
            }

            advance += self.width(code).unwrap_or(FALLBACK_WIDTH);
        }
        Decoded {
            text,
            advance,
            glyphs,
            spaces,
        }
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

    /// Glyph name for a code in a simple font, if an encoding is present.
    fn glyph_name(&self, code: u32) -> Option<&str> {
        self.encoding
            .as_ref()
            .and_then(|e| e.get(code as usize))
            .and_then(|n| n.as_deref())
    }

    fn width(&self, code: u32) -> Option<f64> {
        match &self.widths {
            Widths::Simple { first, w } => {
                let idx = code as i64 - first;
                if idx >= 0 && (idx as usize) < w.len() {
                    return Some(w[idx as usize]);
                }
                // No embedded width for this code: fall back to standard-14 AFM
                // metrics looked up by glyph name (fixes 0.5em-estimate spacing
                // for non-embedded base fonts).
                let afm = self.afm_widths?;
                afm.get(self.glyph_name(code)?).copied()
            }
            Widths::Cid(map) => Some(map.get(&code).copied().unwrap_or(self.default_width)),
        }
    }

    /// Whether the font is bold.
    pub fn is_bold(&self) -> bool {
        self.bold
    }
}

/// Detect bold from the BaseFont name or the FontDescriptor (Type0 fonts carry
/// it on the descendant). Name-based (`-Bold`, `Black`, `Heavy`, `Semibold`) is
/// the most reliable signal; descriptor `Flags` bit 19 (ForceBold) is a backup.
fn font_is_bold(doc: &PdfDocument, fd: &Dictionary) -> bool {
    let base = name_of(fd, b"BaseFont").unwrap_or_default().to_ascii_lowercase();
    if ["bold", "black", "heavy", "semibold", "-bd", ",bd"].iter().any(|k| base.contains(k)) {
        return true;
    }
    // FontDescriptor (directly, or on the descendant CID font for Type0).
    let descr = font_descriptor(doc, fd);
    if let Some(d) = descr {
        if let Some(flags) = int_of(d, b"Flags") {
            if flags & (1 << 18) != 0 {
                return true; // ForceBold
            }
        }
        if let Some(w) = name_of(d, b"FontWeight") {
            if w.parse::<f64>().map(|v| v >= 600.0).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Resolve a font's `/FontDescriptor`, following `/DescendantFonts` for Type0.
fn font_descriptor<'a>(doc: &'a PdfDocument, fd: &'a Dictionary) -> Option<&'a Dictionary> {
    if let Some(Object::Dictionary(d)) = fd.get(b"FontDescriptor").ok().and_then(|o| deref(doc, o)) {
        return Some(d);
    }
    let Some(Object::Array(desc)) = fd.get(b"DescendantFonts").ok().and_then(|o| deref(doc, o))
    else {
        return None;
    };
    let Some(Object::Dictionary(cid)) = desc.first().and_then(|o| deref(doc, o)) else {
        return None;
    };
    match cid.get(b"FontDescriptor").ok().and_then(|o| deref(doc, o)) {
        Some(Object::Dictionary(d)) => Some(d),
        _ => None,
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

    let mut encoding = None;
    let mut afm_widths = None;

    let (widths, default_width) = if is_type0 {
        cid_widths(doc, fd)
    } else {
        encoding = Some(simple_encoding(doc, fd));
        let first = int_of(fd, b"FirstChar").unwrap_or(0);
        let w: Vec<f64> = array_of(doc, fd, b"Widths")
            .map(|a| a.iter().filter_map(num).collect())
            .unwrap_or_default();
        // No embedded widths → try standard-14 AFM metrics by BaseFont name.
        if w.is_empty() {
            if let Some(base) = name_of(fd, b"BaseFont") {
                afm_widths = stdmetrics::widths_for(&base);
            }
        }
        (Widths::Simple { first, w }, 0.0)
    };

    FontInfo {
        is_type0,
        to_unicode,
        widths,
        default_width,
        encoding,
        afm_widths,
        bold: font_is_bold(doc, fd),
    }
}

/// Resolve a simple font's `code -> glyph name` table from its `/Encoding`
/// (a predefined name, or a dict with `/BaseEncoding` + `/Differences`).
/// Absent encoding defaults to StandardEncoding.
fn simple_encoding(doc: &PdfDocument, fd: &Dictionary) -> Vec<Option<String>> {
    let enc = fd.get(b"Encoding").ok().and_then(|o| deref(doc, o));
    match enc {
        Some(Object::Name(n)) => {
            let base = encoding::base_table(&String::from_utf8_lossy(n));
            encoding::build_encoding(base, &[])
        }
        Some(Object::Dictionary(ed)) => {
            let base_name = name_of(ed, b"BaseEncoding").unwrap_or_default();
            let base = encoding::base_table(&base_name);
            let diffs = parse_differences(doc, ed);
            encoding::build_encoding(base, &diffs)
        }
        // No /Encoding: default to StandardEncoding. TODO: a non-symbolic
        // TrueType font without /Encoding should use its built-in cmap (often
        // WinAnsi-equivalent); Standard is a safe approximation for the Type1
        // base fonts this path mainly serves, and only matters without ToUnicode.
        _ => encoding::build_encoding(encoding::base_table("StandardEncoding"), &[]),
    }
}

/// Extract a `/Differences` array (integer code resets + glyph names).
fn parse_differences(doc: &PdfDocument, ed: &Dictionary) -> Vec<Diff> {
    let mut out = Vec::new();
    if let Some(arr) = array_of(doc, ed, b"Differences") {
        for o in arr {
            match o {
                Object::Integer(i) => out.push(Diff::Code(*i as u32)),
                Object::Real(r) => out.push(Diff::Code(*r as u32)),
                Object::Name(n) => out.push(Diff::Name(String::from_utf8_lossy(n).into_owned())),
                _ => {}
            }
        }
    }
    out
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

fn resolve_resources(doc: &PdfDocument, page_id: ObjectId) -> Option<&Dictionary> {
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
