//! One-time model download for the default PP-OCRv6 OCR tier.
//!
//! The binary needs no models to run; OCR is opt-in. When `--ocr` is asked for
//! but `models/ppocr-v6/` is empty, the CLI offers to fetch the raw HuggingFace
//! ONNX here (interactive y/N — non-interactive faces degrade to a clear error;
//! see the caller). Files land under loader-matchable names so `PpOcrEnhancer`
//! reads them directly — no offline static-ize step (`onnx_loader` handles the
//! dynamic graph, `load_dict` reads the dict out of the rec yml).
//!
//! Everything pulled is Apache-2.0; we redistribute nothing.

use anyhow::{Context, Result};
use std::path::Path;

/// `(url, destination filename)` for the four files the v6 tier needs. Names
/// match the loader's `find_file` patterns (*det*.onnx / *rec*.onnx / *rec*.yml
/// for the dict / *cls*.onnx). The cls is v4's — v6 ships no new one.
const FILES: &[(&str, &str)] = &[
    (
        "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/resolve/main/inference.onnx",
        "PP-OCRv6_tiny_det.onnx",
    ),
    (
        "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.onnx",
        "PP-OCRv6_tiny_rec.onnx",
    ),
    (
        "https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/resolve/main/inference.yml",
        "PP-OCRv6_tiny_rec.yml",
    ),
    (
        "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv1/ch_ppocr_mobile_v2.0_cls_infer.onnx",
        "ch_ppocr_mobile_v2.0_cls_infer.onnx",
    ),
];

/// True when `dir` already holds a usable OCR model set (det + rec present).
/// The dict (txt or rec yml) and cls are resolved by the loader; det+rec are
/// the minimum that makes a download unnecessary.
pub fn models_present(dir: &Path) -> bool {
    crate::find_file(dir, &["ch_PP-OCRv4_det_infer.onnx"], "det", ".onnx").is_ok()
        && crate::find_file(dir, &["ch_PP-OCRv4_rec_infer.onnx"], "rec", ".onnx").is_ok()
}

/// Whether `dir` is the built-in PP-OCRv6 default — the only dir we know
/// download URLs for. A custom `--ocr-models` path we can't fetch for.
pub fn is_default_v6_dir(dir: &Path) -> bool {
    dir.file_name().and_then(|n| n.to_str()) == Some("ppocr-v6")
}

/// Download the PP-OCRv6 tiny files into `dir` (~7 MB). Each file streams to a
/// temp sibling, is size-checked, then atomically renamed — an interrupted
/// fetch never leaves a half-written model the loader would choke on. Each file
/// is retried a few times: HF's CDN occasionally drops a connection mid-stream
/// ("response body closed before all bytes were read"), which a retry clears.
/// `progress` is called once per file with a human-readable label.
pub fn fetch_ppocr_v6(dir: &Path, mut progress: impl FnMut(&str)) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .user_agent(concat!("docparse-rs/", env!("CARGO_PKG_VERSION")))
        .build();
    for (url, name) in FILES {
        progress(name);
        let dest = dir.join(name);
        let tmp = dir.join(format!(".{name}.partial"));
        download_one(&agent, url, name, &tmp).with_context(|| format!("download {name}"))?;
        std::fs::rename(&tmp, &dest).with_context(|| format!("install {}", dest.display()))?;
    }
    Ok(())
}

/// Stream `url` to `tmp`, retrying a flaky CDN up to 3 times. The temp file is
/// recreated each attempt so a truncated body never carries over.
fn download_one(agent: &ureq::Agent, url: &str, name: &str, tmp: &Path) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=3 {
        let result = (|| -> Result<u64> {
            let resp = agent
                .get(url)
                .call()
                .with_context(|| format!("GET {url}"))?;
            let mut file =
                std::fs::File::create(tmp).with_context(|| format!("create {}", tmp.display()))?;
            let n = std::io::copy(&mut resp.into_reader(), &mut file)?;
            anyhow::ensure!(n > 1024, "{name} too small ({n} bytes) — source moved?");
            Ok(n)
        })();
        match result {
            Ok(_) => return Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(tmp);
                last_err = Some(e);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(500 * attempt));
                }
            }
        }
    }
    Err(last_err.unwrap()).context("3 attempts failed")
}
