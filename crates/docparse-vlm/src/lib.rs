//! VLM enhancer over OpenAI-compatible services (plan G8b).
//!
//! One protocol covers vLLM, Ollama, LM Studio and cloud endpoints:
//! `POST {url}/v1/chat/completions` with a base64 PNG data-URL image. The
//! deterministic pipeline never depends on this — tasks are opt-in per call,
//! results come back as [`TextChunk`]s with `source: "vlm:<model>"` and
//! reduced confidence, and any service failure degrades to "no annotation",
//! never a parse failure.
//!
//! First task — picture description: each sizable figure region is cropped
//! from an on-demand page render (`docparse-raster`, works for embedded
//! rasters *and* vector-drawn charts alike) and captioned by the model. The
//! caption is injected at the figure's position so text/markdown/chunks all
//! see it in reading order.

use anyhow::{Context, Result};
use base64::Engine;
use docparse_core::ir::{BBox, Document, Element, TextChunk};

/// Minimum share of the page area for a figure to be worth a VLM call.
const MIN_FIGURE_COVERAGE: f32 = 0.01;
/// Page render scale for cropping figure regions (pixels per PDF point).
const RENDER_SCALE: f32 = 2.0;
/// Network timeout per VLM call.
const TIMEOUT_SECS: u64 = 120;
/// Crops are downscaled to this max side before encoding: VLMs don't need
/// more, and an unscaled full-page figure would base64 to ~8MB.
const MAX_IMAGE_SIDE: usize = 1024;

const DESCRIBE_PROMPT: &str = "Describe this figure from a document in one or two sentences. \
     If it is a chart or diagram, state what it shows, including axes and key values. \
     Reply with the description only.";

#[derive(Debug, Clone)]
pub struct VlmConfig {
    /// Service base URL, e.g. `http://127.0.0.1:11434` (Ollama) or a vLLM host.
    pub url: String,
    /// Model name as the service knows it, e.g. `qwen2.5vl` / `llava`.
    pub model: String,
    pub api_key: Option<String>,
}

pub struct VlmClient {
    cfg: VlmConfig,
    agent: ureq::Agent,
}

impl VlmClient {
    pub fn new(cfg: VlmConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .build();
        Self { cfg, agent }
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// One image + prompt → model text. RGB input is PNG-encoded in process.
    pub fn ask_about_image(&self, rgb: &[u8], w: u32, h: u32, prompt: &str) -> Result<String> {
        let (w, h, rgb) = downscale_max(rgb, w as usize, h as usize, MAX_IMAGE_SIDE);
        let (w, h) = (w as u32, h as u32);
        let png = encode_png_rgb(&rgb, w, h);
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        let body = serde_json::json!({
            "model": self.cfg.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
        });
        let url = format!("{}/v1/chat/completions", self.cfg.url.trim_end_matches('/'));
        let mut req = self
            .agent
            .post(&url)
            .set("content-type", "application/json");
        if let Some(key) = &self.cfg.api_key {
            req = req.set("authorization", &format!("Bearer {key}"));
        }
        let resp: serde_json::Value = req
            .send_string(&body.to_string())
            .map_err(|e| anyhow::anyhow!("vlm request failed: {e}"))?
            .into_json()
            .context("vlm response is not JSON")?;
        let text = resp["choices"][0]["message"]["content"]
            .as_str()
            .context("vlm response missing choices[0].message.content")?
            .trim()
            .to_string();
        anyhow::ensure!(!text.is_empty(), "vlm returned an empty description");
        Ok(text)
    }
}

/// Caption sizable figures and inject each caption as a positioned
/// [`TextChunk`] (`source: "vlm:<model>"`). Returns the number of captions.
/// Per-figure failures are reported on stderr and skipped — the deterministic
/// result always stands.
pub fn annotate_pictures(
    doc: &mut Document,
    pdf_bytes: Vec<u8>,
    client: &VlmClient,
) -> Result<usize> {
    let raster = docparse_raster::Rasterizer::new(pdf_bytes)?;
    let mut annotated = 0usize;
    for page in &mut doc.pages {
        let page_area = (page.width * page.height).max(1.0);
        let figures: Vec<BBox> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Image(i) => {
                    let a = (i.bbox.x1 - i.bbox.x0) * (i.bbox.y1 - i.bbox.y0);
                    (a / page_area >= MIN_FIGURE_COVERAGE).then_some(i.bbox)
                }
                _ => None,
            })
            .collect();
        if figures.is_empty() {
            continue;
        }
        let (w, h, rgb) = match raster.render_rgb(page.number.saturating_sub(1), RENDER_SCALE) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("vlm: render failed on page {}: {e:#}", page.number);
                continue;
            }
        };
        for bbox in figures {
            let Some((cw, ch, crop)) = crop_region(
                &rgb,
                w as usize,
                h as usize,
                &bbox,
                RENDER_SCALE,
                page.height,
            ) else {
                continue;
            };
            match client.ask_about_image(&crop, cw as u32, ch as u32, DESCRIBE_PROMPT) {
                Ok(text) => {
                    page.elements.push(Element::Text(TextChunk {
                        text: format!("[figure] {text}"),
                        bbox,
                        font_size: 10.0,
                        font: None,
                        page: page.number,
                        // Model output: capped confidence, audited source.
                        confidence: 0.8,
                        bold: false,
                        hidden: false,
                        source: Some(format!("vlm:{}", client.model())),
                        group: None,
                    }));
                    annotated += 1;
                }
                Err(e) => eprintln!("vlm: describe failed on page {}: {e:#}", page.number),
            }
        }
    }
    Ok(annotated)
}

/// Crop a PDF-space bbox out of a page render (y flips between spaces).
fn crop_region(
    rgb: &[u8],
    w: usize,
    h: usize,
    bbox: &BBox,
    scale: f32,
    page_h: f32,
) -> Option<(usize, usize, Vec<u8>)> {
    let x0 = ((bbox.x0 * scale) as usize).min(w);
    let x1 = ((bbox.x1 * scale) as usize).min(w);
    let y0 = (((page_h - bbox.y1) * scale) as usize).min(h);
    let y1 = (((page_h - bbox.y0) * scale) as usize).min(h);
    let (cw, ch) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if cw < 16 || ch < 16 {
        return None;
    }
    let mut out = vec![0u8; cw * ch * 3];
    for y in 0..ch {
        let src = ((y0 + y) * w + x0) * 3;
        out[y * cw * 3..(y + 1) * cw * 3].copy_from_slice(&rgb[src..src + cw * 3]);
    }
    Some((cw, ch, out))
}

/// Downscale (nearest-neighbour) so the longer side is at most `max_side`.
fn downscale_max(rgb: &[u8], w: usize, h: usize, max_side: usize) -> (usize, usize, Vec<u8>) {
    if w.max(h) <= max_side {
        return (w, h, rgb.to_vec());
    }
    let r = max_side as f32 / w.max(h) as f32;
    let (dw, dh) = (
        ((w as f32 * r) as usize).max(1),
        ((h as f32 * r) as usize).max(1),
    );
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let sy = (y * h / dh).min(h - 1);
        for x in 0..dw {
            let sx = (x * w / dw).min(w - 1);
            let (s, d) = ((sy * w + sx) * 3, (y * dw + x) * 3);
            out[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    (dw, dh, out)
}

// ---------------------------------------------------------------------------
// Minimal PNG encoder (RGB8, stored/uncompressed deflate) — keeps the crate
// free of image/compression dependencies; VLM payloads don't need small files.
// ---------------------------------------------------------------------------

fn encode_png_rgb(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    // Raw scanlines, each prefixed with filter byte 0 (None).
    let stride = (w as usize) * 3;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * stride..(y + 1) * stride]);
    }
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut png, b"IEND", &[]);
    png
}

fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(tag);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// zlib stream using stored (uncompressed) deflate blocks + adler32.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(0xFFFF).peekable();
    while let Some(c) = chunks.next() {
        out.push(if chunks.peek().is_none() { 1 } else { 0 }); // BFINAL
        out.extend_from_slice(&(c.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        out.extend_from_slice(c);
    }
    // adler32
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

struct Crc32(u32);
impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }
    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }
    fn finish(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn png_is_well_formed_and_decodable() {
        let mut rgb = vec![200u8; 4 * 4 * 3];
        rgb[0] = 7; // distinct first pixel survives the roundtrip
        let png = encode_png_rgb(&rgb, 4, 4);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // Decode the stored-deflate IDAT back and verify scanlines + adler32.
        let idat_at = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[idat_at - 4..idat_at].try_into().unwrap()) as usize;
        let z = &png[idat_at + 4..idat_at + 4 + len];
        let mut raw = Vec::new();
        let mut i = 2; // skip zlib header
        loop {
            let bfinal = z[i] & 1;
            let blen = u16::from_le_bytes([z[i + 1], z[i + 2]]) as usize;
            assert_eq!(
                !u16::from_le_bytes([z[i + 1], z[i + 2]]),
                u16::from_le_bytes([z[i + 3], z[i + 4]]),
                "NLEN check"
            );
            raw.extend_from_slice(&z[i + 5..i + 5 + blen]);
            i += 5 + blen;
            if bfinal == 1 {
                break;
            }
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        let adler = u32::from_be_bytes(z[i..i + 4].try_into().unwrap());
        assert_eq!(adler, (b << 16) | a, "adler32 must match");
        // scanlines: filter byte 0 + 12 bytes per row, 4 rows
        assert_eq!(raw.len(), (1 + 12) * 4);
        assert_eq!(raw[0], 0);
        assert_eq!(raw[1], 7, "pixel data survives");
    }

    #[test]
    fn downscale_caps_long_side() {
        let rgb = vec![10u8; 2048 * 100 * 3];
        let (w, h, out) = downscale_max(&rgb, 2048, 100, 1024);
        assert_eq!(w, 1024);
        assert!((49..=50).contains(&h));
        assert_eq!(out.len(), w * h * 3);
        assert!(out.iter().all(|&v| v == 10));
        // No-op below the cap.
        let (w2, h2, _) = downscale_max(&rgb[..30 * 30 * 3], 30, 30, 1024);
        assert_eq!((w2, h2), (30, 30));
    }

    #[test]
    fn crop_flips_y_and_bounds() {
        // 10x10 page rendered at scale 1; bbox occupies the TOP-left quarter
        // in PDF coords (y0=5..y1=10) = top rows of the image.
        let mut rgb = vec![0u8; 10 * 10 * 3];
        for y in 0..5 {
            for x in 0..5 {
                rgb[(y * 10 + x) * 3] = 255;
            }
        }
        let bbox = BBox {
            x0: 0.0,
            y0: 5.0,
            x1: 5.0,
            y1: 10.0,
        };
        // 16px minimum prevents this small crop; use a bigger synthetic page.
        assert!(crop_region(&rgb, 10, 10, &bbox, 1.0, 10.0).is_none());
        let mut big = vec![9u8; 100 * 100 * 3];
        // Mark the image-space top-left pixel (= PDF top-left of the page).
        big[0] = 255;
        let bbox = BBox {
            x0: 0.0,
            y0: 50.0,
            x1: 50.0,
            y1: 100.0,
        };
        let (cw, ch, crop) = crop_region(&big, 100, 100, &bbox, 1.0, 100.0).unwrap();
        assert_eq!((cw, ch), (50, 50));
        // PDF bbox covering the TOP half maps to image rows 0..50 — the marked
        // pixel must be inside the crop at (0,0).
        assert_eq!(crop[0], 255, "y-flip must select the top of the image");
    }

    /// Spin a one-shot HTTP server on a thread, assert the request shape, and
    /// return a canned OpenAI-style response — protocol pinned without any
    /// external service.
    #[test]
    fn client_speaks_openai_protocol() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let mut total = 0usize;
            // Read until headers + body (Content-Length) are in.
            loop {
                let n = s.read(&mut buf[total..]).unwrap();
                total += n;
                let text = String::from_utf8_lossy(&buf[..total]).into_owned();
                if let Some(hdr_end) = text.find("\r\n\r\n") {
                    let cl: usize = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if total >= hdr_end + 4 + cl {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf[..total]).into_owned();
            assert!(text.starts_with("POST /v1/chat/completions"));
            assert!(text
                .to_ascii_lowercase()
                .contains("authorization: bearer k123"));
            let body_start = text.find("\r\n\r\n").unwrap() + 4;
            let body: serde_json::Value = serde_json::from_str(&text[body_start..]).unwrap();
            assert_eq!(body["model"], "test-vlm");
            assert!(body["messages"][0]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
            let resp_body =
                r#"{"choices":[{"message":{"role":"assistant","content":"A bar chart."}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            s.write_all(resp.as_bytes()).unwrap();
        });

        let client = VlmClient::new(VlmConfig {
            url: format!("http://127.0.0.1:{port}"),
            model: "test-vlm".into(),
            api_key: Some("k123".into()),
        });
        let rgb = vec![128u8; 32 * 32 * 3];
        let out = client.ask_about_image(&rgb, 32, 32, "describe").unwrap();
        assert_eq!(out, "A bar chart.");
        handle.join().unwrap();
    }
}
