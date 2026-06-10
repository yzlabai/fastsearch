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
//! Supported payloads (MVP): DCTDecode passthrough (JPEG bytes as-is) and
//! Flate/ASCII85 raw bitmaps with 8 bpc, 1 or 3 components (Gray8/Rgb8).
//! TODO: JBIG2/CCITT/JPX scans are recorded position-only (`ImageKind::None`)
//! — affected pages keep an auditable Image element but can't be OCR'd yet.

use docparse_core::ir::ImageKind;
use lopdf::{Document as PdfDocument, Object, ObjectId, Stream};
use std::collections::HashMap;

/// An undecoded image XObject, resolved off the shared document.
pub struct XImage {
    pub width: u32,
    pub height: u32,
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
}

/// Resolve all image XObjects reachable from a page's resources, keyed by the
/// resource name used by `Do`.
pub fn build_page_images(doc: &PdfDocument, page_id: ObjectId) -> HashMap<String, XImage> {
    let mut out = HashMap::new();
    let Ok(page) = doc.get_dictionary(page_id) else {
        return out;
    };
    let Some(res) = page
        .get(b"Resources")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
    else {
        return out;
    };
    let Some(xobjs) = res
        .get(b"XObject")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_dict().ok())
    else {
        return out;
    };
    for (name, obj) in xobjs.iter() {
        let Ok((_, Object::Stream(s))) = doc.dereference(obj) else {
            continue;
        };
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
        let bpc = s
            .dict
            .get(b"BitsPerComponent")
            .and_then(|o| o.as_i64())
            .unwrap_or(8);
        if width <= 0 || height <= 0 || bpc != 8 {
            continue; // TODO: 1-bit (CCITT/JBIG2) scans — position-only for now
        }
        out.insert(
            String::from_utf8_lossy(name).into_owned(),
            XImage {
                width: width as u32,
                height: height as u32,
                stream: s.clone(),
            },
        );
    }
    out
}
