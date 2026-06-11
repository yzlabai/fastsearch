//! Image XObject collection for the OCR enhancer path (plan N3 / P4 route).
//!
//! Scanned pages are embedded raster images — no rendering is needed to OCR
//! them, only the *original* image bytes (the "structure extraction, not
//! rasterization" identity holds). This module resolves a page's image
//! XObjects up-front (like fonts) so interpretation can run on worker threads,
//! but keeps the streams *undecoded*: pixels are only materialized at `Do`
//! time for page-covering images (scan candidates), so image-heavy digital
//! documents pay nothing.
//!
//! Supported payloads: DCTDecode passthrough (JPEG bytes as-is), Flate/ASCII85
//! raw bitmaps with 8 bpc, 1 or 3 components (Gray8/Rgb8), and 1-bit bi-level
//! scans — CCITT G3/G4 (`hayro-ccitt`), JBIG2 (`hayro-jbig2`) and packed
//! 1-bit Flate — expanded to Gray8 for the OCR path (H1, 2026-06-11).
//! TODO: JPX (JPEG 2000) scans are still recorded position-only
//! (`ImageKind::None`) — auditable Image element, no OCR; no sample seen yet.
//! Form XObjects are resolved too (G4): each form carries its own content
//! stream, /Matrix and resources (fonts/images/nested forms, resolved up
//! front with a depth cap against cycles) — the interpreter executes them
//! recursively so text and scans inside forms are no longer missed.

use crate::font::{build_fonts_from_resources, FontInfo};
use crate::matrix::Matrix;
use docparse_core::ir::ImageKind;
use lopdf::{Dictionary, Document as PdfDocument, Object, ObjectId, Stream};
use std::collections::HashMap;

/// Maximum Form XObject nesting resolved at build time (cycle guard).
pub const MAX_FORM_DEPTH: usize = 4;

/// A Form XObject with its own content stream and pre-resolved resources.
pub struct FormX {
    pub content: Vec<u8>,
    pub matrix: Matrix,
    pub fonts: HashMap<String, FontInfo>,
    pub images: HashMap<String, XImage>,
    pub forms: HashMap<String, FormX>,
}

/// An undecoded image XObject, resolved off the shared document.
pub struct XImage {
    pub width: u32,
    pub height: u32,
    /// 1 for bi-level scans (CCITT/JBIG2/packed Flate), 8 otherwise.
    bpc: u8,
    /// Resolved /DecodeParms for the outermost filter (kept for bi-level only;
    /// `decode()` runs on worker threads without document access).
    parms: Option<Dictionary>,
    /// /JBIG2Globals stream bytes, resolved at build time for the same reason.
    jbig2_globals: Option<Vec<u8>>,
    /// /Decode [1 0] flips sample polarity (PDF 32000-1 Table 89; 1-bit only).
    invert: bool,
    /// The raw stream (filters unapplied) — decoded lazily via [`XImage::decode`].
    stream: Stream,
}

impl XImage {
    /// Materialize the pixel payload. Returns the kind and bytes, or
    /// `ImageKind::None` with empty bytes for unsupported encodings.
    pub fn decode(&self) -> (ImageKind, Vec<u8>) {
        let filters: Vec<String> = self
            .stream
            .filters()
            .map(|fs| {
                fs.iter()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .collect()
            })
            .unwrap_or_default();

        // JPEG passthrough: the common scan encoding. Only the bare chain —
        // a DCT behind ASCII85/Flate pre-filters is rare; TODO if ever seen.
        if filters.last().map(String::as_str) == Some("DCTDecode") {
            if filters.len() == 1 {
                return (ImageKind::Jpeg, self.stream.content.clone());
            }
            return (ImageKind::None, Vec::new());
        }

        // Bi-level scans (fax-class PDFs): expand to Gray8 for the OCR path.
        if self.bpc == 1 {
            return self.decode_bilevel(&filters);
        }

        // Raw bitmap behind Flate/ASCII85/etc.: let lopdf apply the filters,
        // then infer components from the byte count (covers ICCBased RGB too).
        let Ok(pixels) = self.stream.decompressed_content() else {
            return (ImageKind::None, Vec::new());
        };
        let px = (self.width as usize) * (self.height as usize);
        if px == 0 {
            return (ImageKind::None, Vec::new());
        }
        match pixels.len() / px {
            3 if pixels.len() == px * 3 => (ImageKind::Rgb8, pixels),
            1 if pixels.len() == px => (ImageKind::Gray8, pixels),
            _ => (ImageKind::None, Vec::new()),
        }
    }

    /// Decode a bi-level (1-bit) scan into Gray8 (0x00 black / 0xFF white).
    ///
    /// Truncated streams keep the decoded prefix and pad the remainder white —
    /// scans are black-on-white, so padding never invents glyph pixels.
    fn decode_bilevel(&self, filters: &[String]) -> (ImageKind, Vec<u8>) {
        let px = (self.width as usize) * (self.height as usize);
        // Only bare single-filter chains, mirroring the DCT path above —
        // CCITT/JBIG2 behind ASCII85/Flate pre-filters is rare; TODO if seen.
        let gray = match filters.last().map(String::as_str) {
            Some("CCITTFaxDecode") | Some("CCF") if filters.len() == 1 => self.decode_ccitt(),
            Some("JBIG2Decode") if filters.len() == 1 => self.decode_jbig2(),
            Some("CCITTFaxDecode") | Some("CCF") | Some("JBIG2Decode") | Some("JPXDecode") => None,
            // Packed 1-bit bitmap behind Flate/LZW/none: rows byte-padded;
            // DeviceGray maps sample 0 → black (PDF 32000-1 §8.9.5.2).
            _ => self.stream.decompressed_content().ok().map(|bits| {
                let w = self.width as usize;
                let stride = w.div_ceil(8);
                let mut out = Vec::with_capacity(px);
                for row in 0..self.height as usize {
                    let line = bits.get(row * stride..(row + 1) * stride).unwrap_or(&[]);
                    let mut x = 0usize;
                    for i in 0..stride {
                        let b = line.get(i).copied().unwrap_or(0xFF); // missing tail = white
                        let n = (w - x).min(8);
                        for k in 0..n {
                            out.push(if (b >> (7 - k)) & 1 == 1 { 0xFF } else { 0x00 });
                        }
                        x += n;
                    }
                }
                out
            }),
        };
        let Some(mut gray) = gray else {
            return (ImageKind::None, Vec::new());
        };
        // Pad truncated tails / drop overshoot. The pad value is chosen so it
        // reads as WHITE after the optional /Decode inversion below — padding
        // must never invent glyph pixels.
        gray.resize(px, if self.invert { 0x00 } else { 0xFF });
        if self.invert {
            for b in &mut gray {
                *b = !*b;
            }
        }
        (ImageKind::Gray8, gray)
    }

    /// CCITT G3/G4 → Gray8. Parameter mapping per PDF 32000-1 §7.4.6
    /// (defaults: K=0 → G3 1D, Columns=1728, EndOfBlock=true), cross-checked
    /// against hayro-syntax `filter/ccitt.rs` (MIT/Apache-2.0).
    fn decode_ccitt(&self) -> Option<Vec<u8>> {
        use hayro_ccitt::{DecodeSettings, DecoderContext, EncodingMode};

        let parm = self.parms.as_ref();
        let geti = |k: &[u8], d: i64| {
            parm.and_then(|p| p.get(k).ok())
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(d)
        };
        let getb = |k: &[u8], d: bool| {
            parm.and_then(|p| p.get(k).ok())
                .and_then(|o| o.as_bool().ok())
                .unwrap_or(d)
        };
        let columns = geti(b"Columns", 1728).max(1) as u32;
        if columns != self.width {
            // TODO: Columns≠Width would shear row geometry — bail to
            // position-only rather than emit garbage; not seen in the wild.
            return None;
        }
        let k = geti(b"K", 0);
        let settings = DecodeSettings {
            columns,
            rows: geti(b"Rows", self.height as i64).max(0) as u32,
            end_of_block: getb(b"EndOfBlock", true),
            end_of_line: getb(b"EndOfLine", false),
            rows_are_byte_aligned: getb(b"EncodedByteAlign", false),
            encoding: match k {
                k if k < 0 => EncodingMode::Group4,
                0 => EncodingMode::Group3_1D,
                k => EncodingMode::Group3_2D { k: k as u32 },
            },
            invert_black: getb(b"BlackIs1", false),
        };

        let mut sink = Gray8Sink(Vec::with_capacity(
            (self.width as usize) * (self.height as usize),
        ));
        let mut ctx = DecoderContext::new(settings);
        let result = hayro_ccitt::decode(&self.stream.content, &mut sink, &mut ctx);
        // Lenient like hayro: a malformed tail still yields the decoded rows.
        if result.is_err() && sink.0.is_empty() {
            return None;
        }
        Some(sink.0)
    }

    /// Embedded JBIG2 (ITU-T T.88) → Gray8, with /JBIG2Globals when present.
    fn decode_jbig2(&self) -> Option<Vec<u8>> {
        let img =
            hayro_jbig2::Image::new_embedded(&self.stream.content, self.jbig2_globals.as_deref())
                .ok()?;
        let mut sink = Gray8Sink(Vec::with_capacity(
            (self.width as usize) * (self.height as usize),
        ));
        img.decode(&mut sink).ok()?;
        Some(sink.0)
    }
}

/// Shared Gray8 output sink for the bi-level decoders (white = 0xFF). Both
/// hayro decoder traits push runs of same-colored pixels; `chunk_count` is in
/// 8-pixel chunks per their contracts. Note the inverted parameter polarity:
/// hayro-ccitt pushes `white`, hayro-jbig2 pushes `black`.
struct Gray8Sink(Vec<u8>);

impl hayro_ccitt::Decoder for Gray8Sink {
    fn push_pixel(&mut self, white: bool) {
        self.0.push(if white { 0xFF } else { 0x00 });
    }
    fn push_pixel_chunk(&mut self, white: bool, chunk_count: u32) {
        let byte = if white { 0xFF } else { 0x00 };
        self.0
            .extend(std::iter::repeat_n(byte, chunk_count as usize * 8));
    }
    fn next_line(&mut self) {}
}

impl hayro_jbig2::Decoder for Gray8Sink {
    fn push_pixel(&mut self, black: bool) {
        self.0.push(if black { 0x00 } else { 0xFF });
    }
    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        let byte = if black { 0x00 } else { 0xFF };
        self.0
            .extend(std::iter::repeat_n(byte, chunk_count as usize * 8));
    }
    fn next_line(&mut self) {}
}

/// Resolve image XObjects from a page's resources, keyed by `Do` name.
pub fn build_page_images(doc: &PdfDocument, page_id: ObjectId) -> HashMap<String, XImage> {
    match page_resources(doc, page_id) {
        Some(res) => build_images_from_resources(doc, &res),
        None => HashMap::new(),
    }
}

/// Resolve Form XObjects (with their own resources, recursively) from a page.
pub fn build_page_forms(doc: &PdfDocument, page_id: ObjectId) -> HashMap<String, FormX> {
    match page_resources(doc, page_id) {
        Some(res) => build_forms_from_resources(doc, &res, 0),
        None => HashMap::new(),
    }
}

fn page_resources(doc: &PdfDocument, page_id: ObjectId) -> Option<Dictionary> {
    let page = doc.get_dictionary(page_id).ok()?;
    page.get(b"Resources")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
        .cloned()
}

fn xobject_streams(doc: &PdfDocument, res: &Dictionary) -> Vec<(String, Stream)> {
    let Some(xobjs) = res
        .get(b"XObject")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
    else {
        return Vec::new();
    };
    xobjs
        .iter()
        .filter_map(|(name, obj)| match doc.dereference(obj) {
            Ok((_, Object::Stream(s))) => {
                Some((String::from_utf8_lossy(name).into_owned(), s.clone()))
            }
            _ => None,
        })
        .collect()
}

fn build_images_from_resources(doc: &PdfDocument, res: &Dictionary) -> HashMap<String, XImage> {
    let mut out = HashMap::new();
    for (name, s) in xobject_streams(doc, res) {
        if s.dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .unwrap_or(b"?")
            != b"Image"
        {
            continue;
        }
        let width = s.dict.get(b"Width").and_then(|o| o.as_i64()).unwrap_or(0);
        let height = s.dict.get(b"Height").and_then(|o| o.as_i64()).unwrap_or(0);
        // ImageMask images default to 1 bpc (PDF 32000-1 §8.9.6.2). Only
        // fax-class masks (CCITT/JBIG2 filtered) are treated as bi-level ink:
        // a mask paints the CURRENT FILL COLOR (untracked here), so admitting
        // e.g. a page-covering Flate watermark stencil would OCR a light-gray
        // "DRAFT" as black text (2026-06-11 review).
        let mask = s
            .dict
            .get(b"ImageMask")
            .and_then(|o| o.as_bool())
            .unwrap_or(false);
        let fax_filtered = s
            .filters()
            .map(|fs| {
                fs.last()
                    .is_some_and(|f| matches!(&f[..], b"CCITTFaxDecode" | b"CCF" | b"JBIG2Decode"))
            })
            .unwrap_or(false);
        let bpc = if mask {
            if !fax_filtered {
                continue; // non-fax stencil masks stay position-less as before
            }
            1
        } else {
            s.dict
                .get(b"BitsPerComponent")
                .and_then(|o| o.as_i64())
                .unwrap_or(8)
        };
        if width <= 0 || height <= 0 || !(bpc == 8 || bpc == 1) {
            continue; // TODO: 2/4/16-bit depths — position-only for now
        }
        // 1-bit Indexed palettes can invert polarity (index 0 may be white);
        // keep those position-only rather than risk negative output.
        // TODO: read the 2-entry palette and map it properly.
        if bpc == 1 && colorspace_is_indexed(doc, &s.dict) {
            continue;
        }
        // Bi-level extras are resolved here because decode() runs on worker
        // threads without document access.
        let parms = if bpc == 1 {
            resolve_decode_parms(doc, &s.dict)
        } else {
            None
        };
        let jbig2_globals = parms.as_ref().and_then(|p| {
            let obj = p.get(b"JBIG2Globals").ok()?;
            let (_, obj) = doc.dereference(obj).ok()?;
            let g = obj.as_stream().ok()?;
            // Raw bytes are only valid when the stream is unfiltered — a
            // filtered stream that fails to decode must be treated as absent,
            // not fed to the JBIG2 parser compressed (2026-06-11 review).
            let filtered = g.filters().map(|f| !f.is_empty()).unwrap_or(false);
            if filtered {
                g.decompressed_content().ok()
            } else {
                Some(g.content.clone())
            }
        });
        // /Decode [1 0] flips polarity. For an ImageMask the default decode is
        // [0 1] with sample 0 painted (usually black ink) — same Gray8 mapping.
        let invert = bpc == 1
            && s.dict
                .get(b"Decode")
                .and_then(|o| o.as_array())
                .ok()
                .and_then(|a| a.first())
                .map(|o| {
                    matches!(o, Object::Integer(1)) || matches!(o, Object::Real(r) if *r > 0.5)
                })
                .unwrap_or(false);
        out.insert(
            name,
            XImage {
                width: width as u32,
                height: height as u32,
                bpc: bpc as u8,
                parms,
                jbig2_globals,
                invert,
                stream: s,
            },
        );
    }
    out
}

/// Whether the image's /ColorSpace is Indexed (palette-based) — directly or
/// behind a reference.
fn colorspace_is_indexed(doc: &PdfDocument, dict: &Dictionary) -> bool {
    dict.get(b"ColorSpace")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok())
        .and_then(|a| a.first())
        .and_then(|o| o.as_name().ok())
        .map(|n| n == b"Indexed" || n == b"I")
        .unwrap_or(false)
}

/// Resolve /DecodeParms (or legacy /DP) to a dictionary. Single-filter chains
/// only — for the array form the first entry is taken, matching the bare
/// chains `decode()` supports.
fn resolve_decode_parms(doc: &PdfDocument, dict: &Dictionary) -> Option<Dictionary> {
    let obj = dict.get(b"DecodeParms").or_else(|_| dict.get(b"DP")).ok()?;
    let (_, obj) = doc.dereference(obj).ok()?;
    match obj {
        Object::Dictionary(d) => Some(d.clone()),
        Object::Array(a) => a
            .first()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_dict().ok())
            .cloned(),
        _ => None,
    }
}

fn build_forms_from_resources(
    doc: &PdfDocument,
    res: &Dictionary,
    depth: usize,
) -> HashMap<String, FormX> {
    let mut out = HashMap::new();
    if depth >= MAX_FORM_DEPTH {
        return out;
    }
    for (name, s) in xobject_streams(doc, res) {
        if s.dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .unwrap_or(b"?")
            != b"Form"
        {
            continue;
        }
        let matrix = match s.dict.get(b"Matrix").ok().and_then(|o| o.as_array().ok()) {
            Some(arr) if arr.len() == 6 => {
                let v: Vec<f64> = arr
                    .iter()
                    .map(|o| match o {
                        Object::Integer(i) => *i as f64,
                        Object::Real(r) => *r as f64,
                        _ => 0.0,
                    })
                    .collect();
                Matrix {
                    a: v[0],
                    b: v[1],
                    c: v[2],
                    d: v[3],
                    e: v[4],
                    f: v[5],
                }
            }
            _ => Matrix::identity(),
        };
        let Ok(content) = s.decompressed_content() else {
            continue;
        };
        // A form's own resources; fall back to the parent's when absent
        // (allowed by the spec for legacy files).
        let form_res = s
            .dict
            .get(b"Resources")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_dict().ok())
            .cloned()
            .unwrap_or_else(|| res.clone());
        out.insert(
            name,
            FormX {
                content,
                matrix,
                fonts: build_fonts_from_resources(doc, &form_res),
                images: build_images_from_resources(doc, &form_res),
                forms: build_forms_from_resources(doc, &form_res, depth + 1),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ximg(width: u32, height: u32, content: Vec<u8>, invert: bool) -> XImage {
        XImage {
            width,
            height,
            bpc: 1,
            parms: None,
            jbig2_globals: None,
            invert,
            stream: Stream::new(Dictionary::new(), content),
        }
    }

    #[test]
    fn packed_1bit_expands_with_row_padding() {
        // 9px wide → 2-byte stride; bit 1 = white (DeviceGray 0 = black).
        let img = ximg(9, 2, vec![0xAA, 0x80, 0x55, 0x00], false);
        let (kind, px) = img.decode();
        assert_eq!(kind, ImageKind::Gray8);
        assert_eq!(px.len(), 18);
        assert_eq!(
            &px[..9],
            &[0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]
        );
        assert_eq!(&px[9..11], &[0x00, 0xFF]);
    }

    #[test]
    fn packed_1bit_truncated_tail_pads_white() {
        let img = ximg(8, 3, vec![0x00], false); // 1 of 3 rows present
        let (kind, px) = img.decode();
        assert_eq!(kind, ImageKind::Gray8);
        assert_eq!(px.len(), 24);
        assert!(px[..8].iter().all(|&b| b == 0x00));
        assert!(px[8..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn decode_array_inverts_polarity() {
        let img = ximg(8, 1, vec![0xF0], true);
        let (_, px) = img.decode();
        assert_eq!(&px[..], &[0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }
}
