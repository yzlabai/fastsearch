//! A [`RegionReader`] backed by an OpenAI-compatible service — the HTTP twin
//! of the in-process UniRec backend, so `--table-vlm` / `--transcribe-vlm`
//! reuse the exact orchestration (crop → read → reject degenerate → interpret)
//! that the embedded model already goes through.
//!
//! Two things differ from the captioning tasks in this crate, and both matter:
//!
//! * **Resolution.** Captioning downscales to 1024px because a description
//!   doesn't need more. Recognition does — a model with a dynamic resolution
//!   budget (e.g. OvisOCR2's 448²–2880²) would otherwise be handed the very
//!   fixed-resolution ceiling it was picked to escape.
//! * **Concurrency.** These calls are network-bound, so reading a page's
//!   regions one at a time wastes the batching that makes a served model worth
//!   serving. [`RegionReader::read_batch`] is overridden with a small dedicated
//!   pool: enough to keep the service busy, bounded so one ingest process
//!   can't flood a shared endpoint.

use crate::{VlmClient, VlmConfig};
use anyhow::Result;
use docparse_core::region_reader::{RegionImage, RegionReader};
use rayon::prelude::*;

/// Longest image side sent for recognition. Above the captioning default, and
/// within the range page-parsing models accept.
pub const RECOGNITION_MAX_IMAGE_SIDE: usize = 2048;

/// In-flight requests per batch. Enough to feed a served model's batching,
/// small enough that one ingest run doesn't monopolize a shared endpoint.
pub const MAX_INFLIGHT: usize = 4;

const TEXT_PROMPT: &str = "Transcribe all text in this document region in natural reading order. \
     Keep formulas as LaTeX ($...$ inline, $$...$$ display). \
     Output the transcription only — no commentary, no markdown fences.";

const TABLE_PROMPT: &str = "Extract this table as an HTML <table>. Preserve merged cells with \
     rowspan and colspan attributes. Output only the <table>...</table>, no commentary.";

pub struct VlmRegionReader {
    client: VlmClient,
    prompt: &'static str,
    pool: rayon::ThreadPool,
}

impl VlmRegionReader {
    /// Read text-bearing regions (body, titles, captions) — the transcription
    /// backend.
    pub fn for_text(cfg: VlmConfig) -> Result<Self> {
        Self::build(cfg, TEXT_PROMPT)
    }

    /// Re-extract table structure as HTML. HTML rather than TSV on purpose:
    /// the in-process path's `parse_html_table` already expands
    /// `rowspan`/`colspan`, so an HTML answer carries topology a TSV grid
    /// structurally cannot — and needs no new parser.
    pub fn for_table(cfg: VlmConfig) -> Result<Self> {
        Self::build(cfg, TABLE_PROMPT)
    }

    fn build(cfg: VlmConfig, prompt: &'static str) -> Result<Self> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(MAX_INFLIGHT)
            .thread_name(|i| format!("vlm-region-{i}"))
            .build()?;
        Ok(Self {
            client: VlmClient::new(cfg.with_max_image_side(RECOGNITION_MAX_IMAGE_SIDE)),
            prompt,
            pool,
        })
    }
}

impl RegionReader for VlmRegionReader {
    fn read(&self, img: RegionImage<'_>, max_tokens: usize) -> Result<String> {
        self.client
            .ask_about_image(img.rgb, img.w as u32, img.h as u32, self.prompt, max_tokens)
    }

    fn read_batch(&self, imgs: &[RegionImage<'_>], max_tokens: usize) -> Vec<Result<String>> {
        // `par_iter().map().collect()` into a Vec is order-preserving, so the
        // caller can still zip results back onto the regions they came from.
        self.pool.install(|| {
            imgs.par_iter()
                .map(|img| self.read(*img, max_tokens))
                .collect()
        })
    }

    fn source_tag(&self) -> String {
        format!("vlm:{}", self.client.model())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Width/height out of a PNG's IHDR (fixed offsets 16/20), so the stub can
    /// answer with *which image it got* rather than a request counter — the
    /// only way a test can tell a correctly-ordered batch from a shuffled one.
    fn png_dims(png: &[u8]) -> (u32, u32) {
        let at = |o: usize| u32::from_be_bytes(png[o..o + 4].try_into().unwrap());
        (at(16), at(20))
    }

    /// A stub OpenAI endpoint that echoes the dimensions of the image it was
    /// sent. Serves `n` requests then stops.
    fn spawn_stub(n: usize) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);
        let handle = std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut buf = vec![0u8; 1 << 20];
                let mut total = 0usize;
                loop {
                    let Ok(read) = s.read(&mut buf[total..]) else {
                        return;
                    };
                    total += read;
                    let text = String::from_utf8_lossy(&buf[..total]).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let cl: usize = text
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if total >= end + 4 + cl {
                            break;
                        }
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let text = String::from_utf8_lossy(&buf[..total]).into_owned();
                let body_at = text.find("\r\n\r\n").unwrap() + 4;
                let req: serde_json::Value = serde_json::from_str(&text[body_at..]).unwrap();
                let url = req["messages"][0]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap();
                let png = base64::engine::general_purpose::STANDARD
                    .decode(url.trim_start_matches("data:image/png;base64,"))
                    .unwrap();
                let (w, h) = png_dims(&png);
                let body = format!(r#"{{"choices":[{{"message":{{"content":"{w}x{h}"}}}}]}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), served, handle)
    }

    fn reader(url: String) -> VlmRegionReader {
        VlmRegionReader::for_table(VlmConfig::new(url, "stub".into(), None)).unwrap()
    }

    /// T4d — the batch contract, and the one that matters most: results come
    /// back **positionally aligned** with the inputs.
    ///
    /// `transcribe_pages` zips this Vec straight onto the regions it cropped,
    /// so a reordering here would silently attach each region's text to some
    /// *other* region's bbox — wrong citations, no error, no way to notice.
    /// Distinct image sizes + a stub that echoes what it received is what makes
    /// that detectable; a counter-based stub cannot tell the two apart.
    #[test]
    fn read_batch_results_stay_aligned_with_inputs() {
        let (url, served, handle) = spawn_stub(3);
        let big = vec![7u8; 48 * 48 * 3];
        let imgs = vec![
            RegionImage::new(&big[..16 * 16 * 3], 16, 16),
            RegionImage::new(&big[..32 * 32 * 3], 32, 32),
            RegionImage::new(&big, 48, 48),
        ];
        let out = reader(url).read_batch(&imgs, 2000);

        assert_eq!(out.len(), 3, "one result per region, always");
        let texts: Vec<String> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(
            texts,
            ["16x16", "32x32", "48x48"],
            "result[i] must be the answer for imgs[i]"
        );
        assert_eq!(served.load(Ordering::SeqCst), 3);
        handle.join().unwrap();
    }

    /// A dead service must surface as per-item `Err`, never as a panic or a
    /// short batch — the orchestrator's "keep the deterministic result"
    /// fallback depends on getting an error back for that very region.
    #[test]
    fn unreachable_service_yields_errors_not_panics() {
        // Bind then drop: the port is (almost certainly) closed.
        let url = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
        };
        let buf = vec![7u8; 64 * 64 * 3];
        let imgs = vec![
            RegionImage::new(&buf, 64, 64),
            RegionImage::new(&buf, 64, 64),
        ];
        let out = reader(url).read_batch(&imgs, 2000);

        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.is_err()), "failures stay per-item");
    }

    /// A truncated answer must fail, not be parsed. The model does run to the
    /// cap in practice (observed: a blank crop generated `<td>1</td><td>2</td>…`
    /// counting upward until it ran out of budget), and the repetition guard
    /// downstream cannot catch that shape — an incrementing sequence never
    /// repeats literally. `finish_reason` is the signal that does catch it.
    #[test]
    fn answer_cut_off_at_the_cap_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 1 << 20];
            let mut total = 0usize;
            loop {
                let n = s.read(&mut buf[total..]).unwrap();
                total += n;
                let t = String::from_utf8_lossy(&buf[..total]).into_owned();
                if let Some(e) = t.find("\r\n\r\n") {
                    let cl: usize = t
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if total >= e + 4 + cl {
                        break;
                    }
                }
            }
            // Well-formed table *prefix* + truncation flag: the point is that a
            // parseable prefix must not rescue an unfinished answer.
            let body = r#"{"choices":[{"finish_reason":"length","message":{"content":
                "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table><tr><td>5"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        });

        let buf = vec![7u8; 64 * 64 * 3];
        let out =
            reader(format!("http://127.0.0.1:{port}")).read(RegionImage::new(&buf, 64, 64), 2000);
        let err = out
            .expect_err("a cut-off answer must not be accepted")
            .to_string();
        assert!(
            err.contains("cap") && err.contains("2000"),
            "error should name the cap it hit, got: {err}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn source_tag_names_the_model() {
        let r = reader("http://127.0.0.1:1".into());
        assert_eq!(r.source_tag(), "vlm:stub");
    }

    /// Recognition must not inherit the captioning downscale — that ceiling is
    /// precisely what a dynamic-resolution model is chosen to avoid.
    #[test]
    fn recognition_raises_the_image_side_cap() {
        let cfg = VlmConfig::new("http://x".into(), "m".into(), None);
        assert_eq!(cfg.max_image_side, 1024, "captioning default");
        assert_eq!(
            cfg.with_max_image_side(RECOGNITION_MAX_IMAGE_SIDE)
                .max_image_side,
            2048
        );
    }
}
