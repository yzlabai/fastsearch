//! The one seam between "recognize a cropped region" and everything that
//! orchestrates it (table re-extraction, full-page transcription).
//!
//! Both callers already do the same four steps — crop, read, reject degenerate
//! answers, interpret — differing only in how they interpret the string. So the
//! only thing worth abstracting is the *read*: an in-process ONNX model
//! (`UniRec`) and an HTTP service (a VLM) become interchangeable, and neither
//! caller learns which one it got.
//!
//! Concurrency lives behind this trait, not in the callers: an in-process model
//! already saturates the CPU (parallelism would just make it fight itself),
//! while an HTTP backend is network-bound and *must* pipeline to be worth
//! serving. Same call site, opposite right answers — so [`RegionReader::read_batch`]
//! carries a serial default that HTTP backends override.

use anyhow::Result;

/// One cropped region, as packed RGB8 (`w * h * 3` bytes).
#[derive(Debug, Clone, Copy)]
pub struct RegionImage<'a> {
    pub rgb: &'a [u8],
    pub w: usize,
    pub h: usize,
}

impl<'a> RegionImage<'a> {
    pub fn new(rgb: &'a [u8], w: usize, h: usize) -> Self {
        Self { rgb, w, h }
    }
}

/// A recognition backend: region image in, text out.
pub trait RegionReader: Sync {
    /// Read one region. `max_tokens` caps generation and implementors **must**
    /// enforce it — rejecting a runaway after the fact cannot refund the time
    /// already spent producing it.
    fn read(&self, img: RegionImage<'_>, max_tokens: usize) -> Result<String>;

    /// Read a batch, returning results **in the same order and count** as the
    /// input. The default is serial, which is correct for in-process models;
    /// network-bound backends override it with bounded concurrency.
    fn read_batch(&self, imgs: &[RegionImage<'_>], max_tokens: usize) -> Vec<Result<String>> {
        imgs.iter().map(|img| self.read(*img, max_tokens)).collect()
    }

    /// Provenance tag recorded on the produced content (`Table::source`,
    /// `TextChunk::source`), e.g. `"unirec-0.1b"` or `"vlm:OvisOCR2"`.
    fn source_tag(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the sizes it was asked to read, and fails on a chosen index —
    /// enough to pin both the ordering contract and per-item failure isolation.
    struct Echo {
        seen: Mutex<Vec<(usize, usize)>>,
        fail_at: Option<usize>,
    }

    impl RegionReader for Echo {
        fn read(&self, img: RegionImage<'_>, _max_tokens: usize) -> Result<String> {
            let mut seen = self.seen.lock().unwrap();
            let i = seen.len();
            seen.push((img.w, img.h));
            if self.fail_at == Some(i) {
                anyhow::bail!("boom");
            }
            Ok(format!("{}x{}", img.w, img.h))
        }
        fn source_tag(&self) -> String {
            "echo".into()
        }
    }

    fn imgs(buf: &[u8]) -> Vec<RegionImage<'_>> {
        vec![
            RegionImage::new(buf, 1, 1),
            RegionImage::new(buf, 2, 2),
            RegionImage::new(buf, 3, 3),
        ]
    }

    #[test]
    fn read_batch_default_is_ordered_and_complete() {
        let buf = vec![0u8; 27];
        let r = Echo {
            seen: Mutex::new(Vec::new()),
            fail_at: None,
        };
        let out = r.read_batch(&imgs(&buf), 16);
        let texts: Vec<String> = out.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(texts, ["1x1", "2x2", "3x3"], "same order, same count");
    }

    #[test]
    fn read_batch_isolates_per_item_failure() {
        let buf = vec![0u8; 27];
        let r = Echo {
            seen: Mutex::new(Vec::new()),
            fail_at: Some(1),
        };
        let out = r.read_batch(&imgs(&buf), 16);
        assert_eq!(out.len(), 3, "a failure must not shorten the batch");
        assert!(out[0].is_ok());
        assert!(out[1].is_err(), "failure stays at its own index");
        assert_eq!(out[2].as_ref().unwrap(), "3x3");
    }
}
