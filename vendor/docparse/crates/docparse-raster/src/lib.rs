//! On-demand page rasterization for neural enhancers (plan G2; decision in
//! docs/refer/rasterization-options-analysis.md).
//!
//! Positioning note: the *main pipeline never renders pixels* — that is where
//! the parse speed comes from. This crate exists only for pages routed to a
//! neural enhancer (layout/table/VLM), is opt-in, and stays off by default.
//! Rendering is pure Rust via `hayro` (Apache-2.0/MIT), ~100ms/page at 2x.
//!
//! Known upstream limits (hayro 0.7, self-described WIP): non-embedded CID
//! fonts are unsupported; failures here must degrade to "skip enhancement for
//! this page", never fail the parse.

use anyhow::{Context, Result};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{render, RenderCache, RenderSettings};

/// A loaded PDF ready to rasterize individual pages.
pub struct Rasterizer {
    pdf: Pdf,
    interp: InterpreterSettings,
}

impl Rasterizer {
    pub fn new(pdf_bytes: Vec<u8>) -> Result<Self> {
        let pdf = Pdf::new(pdf_bytes).map_err(|e| anyhow::anyhow!("hayro parse: {e:?}"))?;
        Ok(Self {
            pdf,
            interp: InterpreterSettings::default(),
        })
    }

    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Render one page (0-based) at `scale` to tightly-packed RGB8.
    /// Alpha is dropped over the renderer's white background.
    pub fn render_rgb(&self, page_idx: usize, scale: f32) -> Result<(u32, u32, Vec<u8>)> {
        let pages = self.pdf.pages();
        let page = pages.get(page_idx).context("page out of range")?;
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            // hayro defaults to a TRANSPARENT background; premultiplied alpha
            // then reads as black in an RGB view. Documents read on white.
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
            ..Default::default()
        };
        let cache = RenderCache::new();
        let pix = render(page, &cache, &self.interp, &settings);
        let (w, h) = (pix.width() as u32, pix.height() as u32);
        let rgba = pix.data_as_u8_slice();
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        Ok((w, h, rgb))
    }
}

#[cfg(test)]
mod tests {
    use super::Rasterizer;

    #[test]
    fn invalid_pdf_bytes_error_not_panic() {
        // The enhancer path must degrade gracefully: a bad PDF surfaces as an
        // Err (caller skips enhancement for the page), never a panic.
        let err = Rasterizer::new(b"not a pdf at all".to_vec());
        assert!(err.is_err());
    }

    #[test]
    fn empty_bytes_error_not_panic() {
        assert!(Rasterizer::new(Vec::new()).is_err());
    }
}
