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
//! Supported payloads: DCTDecode passthrough (JPEG bytes as-is), JPXDecode
//! (JPEG 2000 → Gray8/Rgb8 via `hayro-jpeg2000`, S2), Flate/ASCII85 raw bitmaps
//! at 8/16/4/2 bpc (16-bit high-byte-downsampled; 2/4-bit DeviceGray unpacked),
//! and 1-bit bi-level scans — CCITT G3/G4 (`hayro-ccitt`), JBIG2 (`hayro-jbig2`)
//! and packed 1-bit Flate, incl. 2-entry Indexed palettes mapped by luminance —
//! all expanded to Gray8/Rgb8 for the OCR path (H1, 2026-06-11; S2, 2026-06-19).
//! Still position-only (auditable Image, no OCR): CMYK JPEG/JPX, ≥3-channel or
//! alpha JPX, RGB sub-byte depths, and Indexed palettes with >2 entries.
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

        // JPEG 2000: a complete codec like DCT (S2). Decode to Gray8/Rgb8;
        // CMYK / ≥4-channel / alpha stay position-only (color-ambiguous).
        if filters.last().map(String::as_str) == Some("JPXDecode") {
            return self.decode_jpx();
        }

        // JPEG passthrough: the common scan encoding. Only the bare chain —
        // a DCT behind ASCII85/Flate pre-filters is rare; TODO if ever seen.
        if filters.last().map(String::as_str) == Some("DCTDecode") {
            if filters.len() == 1 {
                // CMYK/YCCK JPEG (4 components) decoded as RGB downstream comes
                // out color-wrong (and Adobe APP14 often inverts it); record
                // position-only rather than emit a wrong-color image (H7).
                // TODO: decode + APP14-aware CMYK→RGB conversion.
                if jpeg_components(&self.stream.content) == Some(4) {
                    return (ImageKind::None, Vec::new());
                }
                return (ImageKind::Jpeg, self.stream.content.clone());
            }
            return (ImageKind::None, Vec::new());
        }

        // Bi-level scans (fax-class PDFs): expand to Gray8 for the OCR path.
        if self.bpc == 1 {
            return self.decode_bilevel(&filters);
        }

        // Raw bitmap behind Flate/ASCII85/etc.: let lopdf apply the filters,
        // then materialize per bit depth (the build gate admits 8/16 bpc, and
        // 2/4 bpc only for DeviceGray — so sub-byte is always 1 component).
        let Ok(pixels) = self.stream.decompressed_content() else {
            return (ImageKind::None, Vec::new());
        };
        let px = (self.width as usize) * (self.height as usize);
        if px == 0 {
            return (ImageKind::None, Vec::new());
        }
        match self.bpc {
            // 8 bpc: components inferred from the byte count (covers ICCBased
            // RGB too) — byte-aligned, so no row padding to disambiguate.
            8 => match pixels.len() / px {
                3 if pixels.len() == px * 3 => (ImageKind::Rgb8, pixels),
                1 if pixels.len() == px => (ImageKind::Gray8, pixels),
                _ => (ImageKind::None, Vec::new()),
            },
            // 16 bpc: big-endian samples, downsample to 8 by the high byte (S2b).
            16 => downsample_16bit(&pixels, px),
            // 2/4 bpc DeviceGray: unpack MSB-first, row byte-aligned (S2c).
            2 | 4 => {
                unpack_gray_subbyte(&pixels, self.width as usize, self.height as usize, self.bpc)
            }
            _ => (ImageKind::None, Vec::new()),
        }
    }

    /// JPEG 2000 (`JPXDecode`) → Gray8/Rgb8 via `hayro-jpeg2000`. CMYK / ≥4
    /// channels / alpha are color-ambiguous without proper conversion, so they
    /// stay position-only (mirrors the CMYK-JPEG decision in [`XImage::decode`]).
    fn decode_jpx(&self) -> (ImageKind, Vec<u8>) {
        use hayro_jpeg2000::{ColorSpace, DecodeSettings, Image};

        let Ok(img) = Image::new(&self.stream.content, &DecodeSettings::default()) else {
            return (ImageKind::None, Vec::new());
        };
        // Only plain Gray/RGB (or an equivalent ICC/unknown channel count); the
        // library returns 8-bit interleaved samples regardless of source depth.
        let kind = match (img.color_space(), img.has_alpha()) {
            (ColorSpace::Gray, false) => ImageKind::Gray8,
            (ColorSpace::RGB, false) => ImageKind::Rgb8,
            (ColorSpace::Unknown { num_channels: 1 }, false)
            | (
                ColorSpace::Icc {
                    num_channels: 1, ..
                },
                false,
            ) => ImageKind::Gray8,
            (ColorSpace::Unknown { num_channels: 3 }, false)
            | (
                ColorSpace::Icc {
                    num_channels: 3, ..
                },
                false,
            ) => ImageKind::Rgb8,
            _ => return (ImageKind::None, Vec::new()),
        };
        match img.decode() {
            Ok(px) => (kind, px),
            Err(_) => (ImageKind::None, Vec::new()),
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

/// 16-bit raw samples (big-endian, per PDF 32000-1 §7.4.4.2) → 8-bit by taking
/// the high byte. Components inferred from the byte count (byte-aligned at 16
/// bpc, so no row padding): `px*2` = Gray16, `px*6` = Rgb16.
fn downsample_16bit(pixels: &[u8], px: usize) -> (ImageKind, Vec<u8>) {
    let comps = if pixels.len() == px * 2 {
        1
    } else if pixels.len() == px * 6 {
        3
    } else {
        return (ImageKind::None, Vec::new());
    };
    let n = px * comps;
    // High byte of each big-endian sample (samples[2i], 2i+1 dropped).
    let out: Vec<u8> = (0..n).map(|i| pixels[i * 2]).collect();
    (
        if comps == 3 {
            ImageKind::Rgb8
        } else {
            ImageKind::Gray8
        },
        out,
    )
}

/// 2/4-bit DeviceGray samples (MSB-first, rows byte-aligned per PDF 32000-1
/// §7.4.4.2) → Gray8, each value linearly scaled to 0..=255. One component
/// only — the build gate restricts sub-byte depths to DeviceGray.
fn unpack_gray_subbyte(pixels: &[u8], w: usize, h: usize, bpc: u8) -> (ImageKind, Vec<u8>) {
    let bpc = bpc as usize;
    let stride = (w * bpc).div_ceil(8); // bytes per row (1 component)
    let maxv = (1u16 << bpc) - 1; // 3 for 2-bit, 15 for 4-bit
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let line = pixels.get(row * stride..(row + 1) * stride).unwrap_or(&[]);
        let mut bit = 0usize;
        for _ in 0..w {
            let byte = line.get(bit / 8).copied().unwrap_or(0);
            let shift = 8 - bpc - (bit % 8);
            let v = (byte >> shift) & (maxv as u8);
            out.push((v as u16 * 255 / maxv) as u8);
            bit += bpc;
        }
    }
    (ImageKind::Gray8, out)
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

/// Number of color components in a JPEG, from its SOF marker (3 = YCbCr/RGB,
/// 4 = CMYK/YCCK, 1 = grayscale). `None` if no SOF is found.
fn jpeg_components(data: &[u8]) -> Option<u8> {
    let mut i = 2; // skip SOI (FFD8)
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Start-of-frame markers carry the component count; exclude the
        // non-SOF markers in the C0..=CF range (DHT C4, JPG C8, DAC CC).
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return data.get(i + 9).copied(); // FF M len(2) prec h(2) w(2) comps
        }
        // Standalone markers (RSTn, SOI, EOI, TEM) have no length field.
        if matches!(marker, 0xD0..=0xD9 | 0x01) {
            i += 2;
            continue;
        }
        let len = ((data[i + 2] as usize) << 8) | data[i + 3] as usize;
        i += 2 + len.max(2);
    }
    None
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
        // Depths the OCR path can materialize: 8/1 bpc unconditionally, 16 bpc
        // (high-byte downsample), and 2/4 bpc only for DeviceGray (sub-byte RGB
        // is rare; its byte-count is ambiguous under row padding — S2c).
        let depth_ok = bpc == 8
            || bpc == 1
            || bpc == 16
            || ((bpc == 2 || bpc == 4) && colorspace_is_device_gray(doc, &s.dict));
        if width <= 0 || height <= 0 || !depth_ok {
            continue;
        }
        // 1-bit Indexed palettes carry the polarity in the lookup table (index 0
        // may be the light color). Resolve the 2-entry palette to a black/white
        // polarity (S2d) and fold it into `invert`; undeterminable palettes
        // (>2 entries, non-Gray/RGB base) stay position-only.
        let palette_invert = if bpc == 1 && colorspace_is_indexed(doc, &s.dict) {
            match indexed_1bit_invert(doc, &s.dict) {
                Some(inv) => inv,
                None => continue,
            }
        } else {
            false
        };
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
        // For Indexed, the palette polarity (S2d) XORs with any /Decode flip.
        let decode_invert = bpc == 1
            && s.dict
                .get(b"Decode")
                .and_then(|o| o.as_array())
                .ok()
                .and_then(|a| a.first())
                .map(|o| {
                    matches!(o, Object::Integer(1)) || matches!(o, Object::Real(r) if *r > 0.5)
                })
                .unwrap_or(false);
        let invert = decode_invert ^ palette_invert;
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

/// Whether the image's /ColorSpace is DeviceGray (the only space we admit for
/// sub-byte depths — see [`build_images_from_resources`]). A bare `/G` alias or
/// a 1-component ICCBased space both count as gray.
fn colorspace_is_device_gray(doc: &PdfDocument, dict: &Dictionary) -> bool {
    let Some((_, cs)) = dict
        .get(b"ColorSpace")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
    else {
        return false;
    };
    match cs {
        Object::Name(n) => n == b"DeviceGray" || n == b"G",
        // [/ICCBased stream] with /N 1 is gray.
        Object::Array(a) => {
            let is_icc = a.first().and_then(|o| o.as_name().ok()) == Some(b"ICCBased");
            let n1 = a
                .get(1)
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_stream().ok().cloned())
                .and_then(|s| s.dict.get(b"N").ok().and_then(|o| o.as_i64().ok()))
                == Some(1);
            is_icc && n1
        }
        _ => false,
    }
}

/// Resolve a 2-entry (`hival == 1`) Indexed palette to a bi-level polarity:
/// `Some(true)` if index 0 is the *lighter* color (so the packed-bit decoder's
/// "bit set = white" assumption is backwards and must be inverted), `Some(false)`
/// if index 0 is darker (assumption holds). `None` when the palette isn't a
/// plain 2-entry DeviceGray/DeviceRGB table we can read, or the two colors have
/// equal luminance — caller keeps such images position-only.
fn indexed_1bit_invert(doc: &PdfDocument, dict: &Dictionary) -> Option<bool> {
    let arr = dict
        .get(b"ColorSpace")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok().cloned())?;
    // [/Indexed base hival lookup]
    if arr.len() != 4 {
        return None;
    }
    if arr[2].as_i64().ok()? != 1 {
        return None; // only true 2-entry palettes
    }
    let base = doc.dereference(&arr[1]).ok()?.1;
    let comps = match base.as_name().ok()? {
        b"DeviceGray" | b"G" | b"CalGray" => 1usize,
        b"DeviceRGB" | b"RGB" | b"CalRGB" => 3usize,
        _ => return None,
    };
    // Lookup is a byte string or a stream of base-space samples.
    let lookup = match doc.dereference(&arr[3]).ok()?.1 {
        Object::String(s, _) => s.clone(),
        Object::Stream(s) => s.decompressed_content().ok()?,
        _ => return None,
    };
    if lookup.len() < comps * 2 {
        return None;
    }
    // Rec. 601 luma for RGB; the byte itself for gray.
    let luma = |off: usize| -> u32 {
        if comps == 1 {
            lookup[off] as u32
        } else {
            let (r, g, b) = (
                lookup[off] as u32,
                lookup[off + 1] as u32,
                lookup[off + 2] as u32,
            );
            (299 * r + 587 * g + 114 * b) / 1000
        }
    };
    let (l0, l1) = (luma(0), luma(comps));
    if l0 == l1 {
        return None;
    }
    Some(l0 > l1)
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

    #[test]
    fn jpeg_component_count_from_sof() {
        // Minimal SOI + SOF0(len 17, 8-bit, 16×16) with N components.
        let sof = |n: u8| {
            let mut j = vec![
                0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x10, n,
            ];
            j.extend_from_slice(&[0u8; 12]);
            j
        };
        assert_eq!(jpeg_components(&sof(4)), Some(4)); // CMYK
        assert_eq!(jpeg_components(&sof(3)), Some(3)); // YCbCr
        assert_eq!(jpeg_components(&sof(1)), Some(1)); // grayscale
                                                       // SOF reached after an APP0 segment is skipped by length.
        let mut with_app0 = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x06, 1, 2, 3, 4];
        with_app0.extend_from_slice(&sof(4)[2..]);
        assert_eq!(jpeg_components(&with_app0), Some(4));
        assert_eq!(jpeg_components(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn downsample_16bit_takes_high_byte() {
        // 2×1 Gray16, big-endian: 0x12 34, 0xAB CD → high bytes 0x12, 0xAB.
        let (kind, px) = downsample_16bit(&[0x12, 0x34, 0xAB, 0xCD], 2);
        assert_eq!(kind, ImageKind::Gray8);
        assert_eq!(px, vec![0x12, 0xAB]);
        // 1×1 Rgb16 → 3 high bytes.
        let (kind, px) = downsample_16bit(&[0x10, 0, 0x20, 0, 0x30, 0], 1);
        assert_eq!(kind, ImageKind::Rgb8);
        assert_eq!(px, vec![0x10, 0x20, 0x30]);
        // Wrong byte count → position-only.
        assert_eq!(downsample_16bit(&[0x00], 1).0, ImageKind::None);
    }

    #[test]
    fn unpack_subbyte_gray_scales_to_full_range() {
        // 4-bit, 2px wide, 1 row: 0x0F → values 0,15 → 0, 255.
        let (kind, px) = unpack_gray_subbyte(&[0x0F], 2, 1, 4);
        assert_eq!(kind, ImageKind::Gray8);
        assert_eq!(px, vec![0, 255]);
        // 2-bit, 4px: 0b00_01_10_11 → 0,1,2,3 → 0,85,170,255.
        let (_, px) = unpack_gray_subbyte(&[0b00_01_10_11], 4, 1, 2);
        assert_eq!(px, vec![0, 85, 170, 255]);
        // Row padding: 3px 4-bit → 2-byte stride; second row independent.
        let (_, px) = unpack_gray_subbyte(&[0xF0, 0x00, 0x00, 0xF0], 3, 2, 4);
        assert_eq!(px, vec![255, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn device_gray_colorspace_detected() {
        let doc = PdfDocument::new();
        let mut d = Dictionary::new();
        d.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
        assert!(colorspace_is_device_gray(&doc, &d));
        let mut rgb = Dictionary::new();
        rgb.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
        assert!(!colorspace_is_device_gray(&doc, &rgb));
    }

    #[test]
    fn indexed_palette_polarity_from_luminance() {
        let doc = PdfDocument::new();
        let indexed = |entry0: u8, entry1: u8| {
            let mut d = Dictionary::new();
            d.set(
                "ColorSpace",
                Object::Array(vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceGray".to_vec()),
                    Object::Integer(1),
                    Object::String(vec![entry0, entry1], lopdf::StringFormat::Hexadecimal),
                ]),
            );
            d
        };
        // index 0 = black, 1 = white → normal polarity, no invert.
        assert_eq!(indexed_1bit_invert(&doc, &indexed(0x00, 0xFF)), Some(false));
        // index 0 = white, 1 = black → inverted.
        assert_eq!(indexed_1bit_invert(&doc, &indexed(0xFF, 0x00)), Some(true));
        // Equal luminance → undeterminable.
        assert_eq!(indexed_1bit_invert(&doc, &indexed(0x80, 0x80)), None);
    }
}
