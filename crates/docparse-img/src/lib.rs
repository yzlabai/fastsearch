//! Images-as-documents backend (G1b): a PNG/JPEG file becomes a one-page
//! document whose sole element is a full-page [`ImageChunk`] — which is
//! exactly the shape the quality layer flags `ScannedNoText` and the OCR
//! enhancer consumes (`--ocr`). The whole N3 OCR pipeline (routing, PP-OCR
//! inference, positioned text injection) is reused for free; without `--ocr`
//! the output is an auditable image record (dims + bbox).
//!
//! Coordinates: 1 px = 1 pt, page = image size — OCR box mapping is exact.
//! JPEG stays UNDECODED (`ImageKind::Jpeg` carries the file bytes; the OCR
//! path decodes on demand) — same zero-transcode policy as PDF scan pages.
//! PNG decodes to Gray8/Rgb8 (alpha dropped against white is NOT done — the
//! channel is simply discarded; transparent scans are not a real case).
//! 16-bit PNGs and exotic color types are refused with a clear error.

use docparse_core::ir::{BBox, Document, Element, ImageChunk, ImageKind, Page, Provenance};
use docparse_core::parser::DocumentParser;
use std::path::Path;

pub struct ImageParser;

impl DocumentParser for ImageParser {
    fn name(&self) -> &'static str {
        "image"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                e.eq_ignore_ascii_case("png")
                    || e.eq_ignore_ascii_case("jpg")
                    || e.eq_ignore_ascii_case("jpeg")
            })
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let bytes = std::fs::read(path)?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut doc = parse_bytes(&bytes, &ext)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Build the one-page document from raw image bytes.
pub fn parse_bytes(bytes: &[u8], ext: &str) -> anyhow::Result<Document> {
    let (width, height, kind, data) = match ext {
        "png" => decode_png(bytes)?,
        "jpg" | "jpeg" => {
            // Dimensions from the header only; bytes pass through undecoded.
            let mut dec = zune_jpeg::JpegDecoder::new(bytes);
            dec.decode_headers()
                .map_err(|e| anyhow::anyhow!("jpeg header: {e:?}"))?;
            let (w, h) = dec
                .dimensions()
                .ok_or_else(|| anyhow::anyhow!("jpeg without dimensions"))?;
            (w as u32, h as u32, ImageKind::Jpeg, bytes.to_vec())
        }
        other => anyhow::bail!("unsupported image extension: {other}"),
    };

    let page = Page {
        number: 1,
        width: width as f32,
        height: height as f32,
        elements: vec![Element::Image(ImageChunk {
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: width as f32,
                y1: height as f32,
            },
            page: 1,
            width_px: width,
            turns: 0,
            height_px: height,
            kind,
            data,
            file: None,
            data_base64: None,
            data_media_type: None,
        })],
    };
    Ok(Document {
        source: "<image>".to_string(),
        provenance: Some(Provenance::new("image", env!("CARGO_PKG_VERSION"))),
        pages: vec![page],
    })
}

/// PNG → (w, h, Gray8|Rgb8, pixels). Alpha channels are stripped.
fn decode_png(bytes: &[u8]) -> anyhow::Result<(u32, u32, ImageKind, Vec<u8>)> {
    use zune_png::zune_core::colorspace::ColorSpace;
    let mut dec = zune_png::PngDecoder::new(bytes);
    let pixels = dec
        .decode_raw()
        .map_err(|e| anyhow::anyhow!("png decode: {e:?}"))?;
    let (w, h) = dec
        .get_dimensions()
        .ok_or_else(|| anyhow::anyhow!("png without dimensions"))?;
    let cs = dec
        .get_colorspace()
        .ok_or_else(|| anyhow::anyhow!("png without colorspace"))?;
    let px = w * h;
    let (kind, data) = match cs {
        ColorSpace::Luma if pixels.len() == px => (ImageKind::Gray8, pixels),
        ColorSpace::RGB if pixels.len() == px * 3 => (ImageKind::Rgb8, pixels),
        ColorSpace::LumaA if pixels.len() == px * 2 => (
            ImageKind::Gray8,
            pixels.chunks_exact(2).map(|c| c[0]).collect(),
        ),
        ColorSpace::RGBA if pixels.len() == px * 4 => (
            ImageKind::Rgb8,
            pixels
                .chunks_exact(4)
                .flat_map(|c| [c[0], c[1], c[2]])
                .collect(),
        ),
        other => anyhow::bail!("unsupported png layout: {other:?} ({} bytes)", pixels.len()),
    };
    Ok((w as u32, h as u32, kind, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known-good 2×2 8-bit grayscale PNG (generated offline, CRC valid).
    fn tiny_png_gray() -> Vec<u8> {
        const B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAAAAABX3VL4AAAADklEQVR4nGMQUGAwcAAAAXYAoewwivQAAAAASUVORK5CYII=";
        b64(B64)
    }

    fn b64(s: &str) -> Vec<u8> {
        // Tiny base64 decoder for test fixtures only.
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut buf = 0u32;
        let mut nbits = 0;
        for &c in s.as_bytes() {
            if c == b'=' {
                break;
            }
            let v = T.iter().position(|&t| t == c).unwrap() as u32;
            buf = (buf << 6) | v;
            nbits += 6;
            if nbits >= 8 {
                nbits -= 8;
                out.push((buf >> nbits) as u8);
            }
        }
        out
    }

    #[test]
    fn png_becomes_full_page_image() {
        let doc = parse_bytes(&tiny_png_gray(), "png").unwrap();
        assert_eq!(doc.pages.len(), 1);
        let page = &doc.pages[0];
        assert_eq!((page.width, page.height), (2.0, 2.0));
        let Element::Image(img) = &page.elements[0] else {
            panic!("expected image element");
        };
        assert_eq!((img.width_px, img.height_px), (2, 2));
        assert!(matches!(img.kind, ImageKind::Gray8 | ImageKind::Rgb8));
        assert!(!img.data.is_empty());
    }

    #[test]
    fn garbage_is_a_clean_error() {
        assert!(parse_bytes(b"not an image", "png").is_err());
        assert!(parse_bytes(b"not an image", "jpg").is_err());
    }
}
