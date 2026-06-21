//! Synthetic page layout for formats without real coordinates (DOCX, HTML).
//!
//! Backends for structured formats know the *logical* order of paragraphs,
//! headings and tables but have no PDF-style geometry. To keep the IR's single
//! coordinate convention (PDF user space, origin bottom-left, y-up, points), we
//! flow blocks top-to-bottom down a synthetic page with margins, paginating
//! when we run out of vertical space. Reading order and output then work
//! unchanged. Coordinates are an honest fabrication — useful for relative
//! ordering and citation anchoring, not for fidelity to any real page.

use crate::ir::{BBox, Cell, Element, ImageChunk, ImageKind, Page, Table, TextChunk};

/// Flows logical blocks onto synthetic pages.
pub struct PageBuilder {
    width: f32,
    height: f32,
    margin: f32,
    y: f32,
    page_no: usize,
    cur: Vec<Element>,
    pages: Vec<Page>,
}

impl PageBuilder {
    /// US-Letter-ish default page (612×792 pt) with 72 pt margins.
    pub fn letter() -> Self {
        Self::new(612.0, 792.0, 72.0)
    }

    pub fn new(width: f32, height: f32, margin: f32) -> Self {
        Self {
            width,
            height,
            margin,
            y: height - margin,
            page_no: 1,
            cur: Vec::new(),
            pages: Vec::new(),
        }
    }

    fn line_height(size: f32) -> f32 {
        size * 1.4
    }

    /// Start a new page if `need` points don't fit above the bottom margin.
    fn ensure(&mut self, need: f32) {
        if self.y - need < self.margin {
            self.flush_page();
        }
    }

    fn flush_page(&mut self) {
        self.pages.push(Page {
            number: self.page_no,
            width: self.width,
            height: self.height,
            elements: std::mem::take(&mut self.cur),
        });
        self.page_no += 1;
        self.y = self.height - self.margin;
    }

    /// Force a page break (e.g. one slide per page for presentations). No-op
    /// on a still-empty page.
    pub fn page_break(&mut self) {
        if !self.cur.is_empty() {
            self.flush_page();
        }
    }

    /// Estimated text width (avg glyph ≈ 0.5 em), capped to the content width.
    fn text_width(&self, text: &str, size: f32) -> f32 {
        (text.chars().count() as f32 * size * 0.5).min(self.width - 2.0 * self.margin)
    }

    /// Extra vertical space after a paragraph (× font size). One builder
    /// paragraph is already a complete logical block, so consecutive blocks
    /// must NOT look like wrapped lines of one paragraph: the block grouper
    /// merges lines whose center gap is ≤1.8 em, and the bare 1.4 em line
    /// height fell under that — every synthetic backend's paragraphs were
    /// being mashed together in text/markdown output.
    const PARA_SPACING: f32 = 0.6;

    /// Add a paragraph (or heading — just use a larger `size`). Empty text is
    /// skipped. One chunk per block; the output layer reconstructs paragraphs.
    pub fn paragraph(&mut self, text: impl Into<String>, size: f32) {
        self.push_text(text, size, None);
    }

    /// Add a list item (marker included in `text`, e.g. `"• alpha"` /
    /// `"1. one"`). Tagged `LI` so the structured-format backend's knowledge
    /// overrides geometric reclassification — without it, an ordinal item
    /// like "1. First item" reads as a numbered section heading downstream
    /// (the geometric rule cannot tell them apart; the author can).
    pub fn list_item(&mut self, text: impl Into<String>, size: f32) {
        self.push_text(text, size, Some("LI".to_string()));
    }

    fn push_text(&mut self, text: impl Into<String>, size: f32, tag: Option<String>) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        let lh = Self::line_height(size);
        self.ensure(lh);
        let y1 = self.y;
        let y0 = self.y - size;
        let x1 = self.margin + self.text_width(&text, size);
        self.cur.push(Element::Text(TextChunk {
            text,
            bbox: BBox {
                x0: self.margin,
                y0,
                x1,
                y1,
            },
            font_size: size,
            font: None,
            page: self.page_no,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag,
        }));
        self.y -= lh + Self::PARA_SPACING * size;
    }

    /// Add an already-encoded image (PNG/JPEG/… `data`) at the current flow
    /// position. `width_pt`/`height_pt` are the on-page size (converted from the
    /// source's EMU/px by the backend), capped to the content box. `media_type`
    /// is the source MIME (drives export/embed). `caption` is an optional
    /// `(text, source)` description carried straight onto the chunk — backends
    /// with an in-document caption (HTML `alt`) supply it; OOXML pass `None` and
    /// let the chunker bind an adjacent "Figure N" line. Empty `data` is skipped.
    /// Coordinates are synthetic — useful for ordering/citation, not fidelity.
    pub fn image(
        &mut self,
        data: Vec<u8>,
        width_pt: f32,
        height_pt: f32,
        media_type: impl Into<String>,
        caption: Option<(String, &'static str)>,
    ) {
        if data.is_empty() {
            return;
        }
        let content_w = self.width - 2.0 * self.margin;
        let content_h = self.height - 2.0 * self.margin;
        let w = width_pt.clamp(1.0, content_w);
        let h = height_pt.clamp(1.0, content_h);
        self.ensure(h);
        let y1 = self.y;
        let y0 = self.y - h;
        let (caption, caption_source) = match caption {
            Some((c, s)) => (Some(c), Some(s.to_string())),
            None => (None, None),
        };
        self.cur.push(Element::Image(ImageChunk {
            bbox: BBox {
                x0: self.margin,
                y0,
                x1: self.margin + w,
                y1,
            },
            page: self.page_no,
            // px dims unknown (we don't decode); export passes the bytes through.
            width_px: 0,
            height_px: 0,
            turns: 0,
            kind: ImageKind::Encoded,
            data,
            file: None,
            data_base64: None,
            data_media_type: Some(media_type.into()),
            caption,
            caption_source,
        }));
        self.y = y0 - Self::PARA_SPACING * 12.0;
    }

    /// Add a table from row-major cell text (every cell 1×1). Thin wrapper over
    /// [`PageBuilder::table_spanned`] — the no-span path for CSV/Markdown/XLSX/…
    pub fn table(&mut self, rows: Vec<Vec<String>>, size: f32) {
        let sparse = rows
            .into_iter()
            .map(|r| r.into_iter().map(SpanCell::plain).collect())
            .collect();
        self.table_spanned(sparse, size);
    }

    /// Add a table from a SPARSE span grid: covered positions are omitted and
    /// each anchor carries its spans (the shape HTML `colspan`/`rowspan` and
    /// DOCX `gridSpan`/`vMerge` produce). [`expand_spans`] materializes it into
    /// the IR's flat grid — anchors keep their spans, covered positions are
    /// filled with the replicated text and `merged = true`. Cells get synthetic
    /// grid bboxes; anchors span their merged region.
    pub fn table_spanned(&mut self, rows: Vec<Vec<SpanCell>>, size: f32) {
        if rows.is_empty() {
            return;
        }
        let grid = expand_spans(rows);
        let nrows = grid.len();
        let ncols = grid.iter().map(Vec::len).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let row_h = Self::line_height(size) + size * 0.4;
        let total = row_h * nrows as f32;
        self.ensure(total.min(self.height - 2.0 * self.margin));
        let col_w = (self.width - 2.0 * self.margin) / ncols as f32;
        let top = self.y;
        let mut out_rows: Vec<Vec<Cell>> = Vec::with_capacity(nrows);
        for (r, row) in grid.into_iter().enumerate() {
            let y_top = top - r as f32 * row_h;
            let mut cells = Vec::with_capacity(row.len());
            for (c, fc) in row.into_iter().enumerate() {
                let x0 = self.margin + c as f32 * col_w;
                cells.push(Cell {
                    bbox: BBox {
                        x0,
                        // Anchors extend over their span; covered cells are 1×1.
                        y0: y_top - fc.row_span as f32 * row_h,
                        x1: x0 + fc.col_span as f32 * col_w,
                        y1: y_top,
                    },
                    text: fc.text,
                    row_span: fc.row_span,
                    col_span: fc.col_span,
                    merged: fc.merged,
                });
            }
            out_rows.push(cells);
        }
        let bottom = top - total;
        self.cur.push(Element::Table(Table {
            bbox: BBox {
                x0: self.margin,
                y0: bottom,
                x1: self.width - self.margin,
                y1: top,
            },
            page: self.page_no,
            rows: out_rows,
            source: None,
        }));
        self.y = bottom - Self::line_height(size);
    }

    /// Finish, returning all pages (flushing the last if non-empty).
    pub fn finish(mut self) -> Vec<Page> {
        if !self.cur.is_empty() || self.pages.is_empty() {
            self.flush_page();
        }
        self.pages
    }
}

/// EMU (English Metric Units, 914400 per inch) → PDF points (72 per inch).
/// OOXML drawing sizes (DOCX `wp:extent`, PPTX `a:ext`) are in EMU; the
/// synthetic OOXML backends share this conversion.
pub fn emu_to_pt(emu: u32) -> f32 {
    emu as f32 / 12700.0
}

/// MIME type from an image path's extension (the form OOXML media targets —
/// `word/media/imageN.png`, `ppt/media/imageN.jpeg` — and HTML `img` srcs carry).
/// Defaults to `application/octet-stream`.
pub fn image_mime_from_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        "emf" => "image/x-emf",
        "wmf" => "image/x-wmf",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// A source cell in a SPARSE span grid: covered positions are omitted and the
/// anchor carries its spans (the shape HTML `colspan`/`rowspan` and a
/// normalized DOCX `gridSpan`/`vMerge` produce). Expanded into the IR's flat
/// grid by [`PageBuilder::table_spanned`].
pub struct SpanCell {
    pub text: String,
    pub row_span: u32,
    pub col_span: u32,
}

impl SpanCell {
    /// A plain 1×1 cell (no span).
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            row_span: 1,
            col_span: 1,
        }
    }
}

/// One materialized grid position after span expansion.
struct FlatCell {
    text: String,
    row_span: u32,
    col_span: u32,
    merged: bool,
}

/// Expand a sparse span grid into the IR's flat row-major grid: the anchor keeps
/// its spans (`merged = false`); every covered position is materialized with the
/// replicated anchor text and `merged = true`; rows are padded to a rectangle.
///
/// Mirrors [`docparse_ocr::table_model::parse_html_table`]'s pending-rowspan
/// algorithm (the eval/ODL convention) so the synthetic backends and the table
/// model agree on span semantics. (Kept separate to avoid a core→ocr dependency;
/// the two should stay in sync.)
fn expand_spans(sparse: Vec<Vec<SpanCell>>) -> Vec<Vec<FlatCell>> {
    let covered = |text: &str| FlatCell {
        text: text.to_string(),
        row_span: 1,
        col_span: 1,
        merged: true,
    };
    let mut rows: Vec<Vec<FlatCell>> = Vec::with_capacity(sparse.len());
    // pending[col] = (remaining_rows, replicated_text) owed by an earlier rowspan.
    let mut pending: Vec<(u32, String)> = Vec::new();
    for src_row in sparse {
        let mut row: Vec<FlatCell> = Vec::new();
        let mut col = 0usize;
        let mut cells = src_row.into_iter();
        loop {
            // Fill positions owed to earlier rowspans before placing a new cell.
            if let Some(slot) = pending.get_mut(col).filter(|(left, _)| *left > 0) {
                slot.0 -= 1;
                let t = slot.1.clone();
                row.push(covered(&t));
                col += 1;
                continue;
            }
            let Some(cell) = cells.next() else { break };
            let rs = cell.row_span.max(1);
            let cs = cell.col_span.max(1);
            for k in 0..cs {
                if pending.len() <= col {
                    pending.resize(col + 1, (0, String::new()));
                }
                pending[col] = (rs - 1, cell.text.clone());
                row.push(if k == 0 {
                    FlatCell {
                        text: cell.text.clone(),
                        row_span: rs,
                        col_span: cs,
                        merged: false,
                    }
                } else {
                    covered(&cell.text)
                });
                col += 1;
            }
        }
        // Trailing rowspan positions after the row's last explicit cell.
        while col < pending.len() {
            if pending[col].0 > 0 {
                pending[col].0 -= 1;
                let t = pending[col].1.clone();
                row.push(covered(&t));
            }
            col += 1;
        }
        rows.push(row);
    }
    // Pad to a rectangle (short rows get empty 1×1 cells).
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    for r in &mut rows {
        while r.len() < ncols {
            r.push(FlatCell {
                text: String::new(),
                row_span: 1,
                col_span: 1,
                merged: false,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Element;

    #[test]
    fn paragraphs_flow_down_and_paginate() {
        let mut b = PageBuilder::new(200.0, 200.0, 20.0);
        for i in 0..40 {
            b.paragraph(format!("para {i}"), 12.0);
        }
        let pages = b.finish();
        assert!(pages.len() > 1, "should paginate");
        // y decreases within a page; all chunks present.
        let total: usize = pages.iter().map(|p| p.elements.len()).sum();
        assert_eq!(total, 40);
    }

    #[test]
    fn table_becomes_table_element() {
        let mut b = PageBuilder::letter();
        b.paragraph("Title", 20.0);
        b.table(
            vec![vec!["a".into(), "b".into()], vec!["c".into(), "d".into()]],
            12.0,
        );
        let pages = b.finish();
        let tables: Vec<_> = pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter(|e| matches!(e, Element::Table(_)))
            .collect();
        assert_eq!(tables.len(), 1);
    }

    // TC-01: a plain grid expands unchanged; short rows pad to a rectangle.
    #[test]
    fn expand_plain_grid_pads_and_keeps_1x1() {
        let g = expand_spans(vec![
            vec![SpanCell::plain("a"), SpanCell::plain("b")],
            vec![SpanCell::plain("c")],
        ]);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].len(), 2);
        assert_eq!(g[1].len(), 2, "short row padded to ncols");
        assert!(g
            .iter()
            .flatten()
            .all(|c| c.row_span == 1 && c.col_span == 1 && !c.merged));
        assert_eq!(g[1][1].text, "", "pad cell is empty");
    }

    // TC-02: colspan anchor + replicated covered position to its right.
    #[test]
    fn expand_colspan_materializes_covered() {
        let g = expand_spans(vec![
            vec![SpanCell {
                text: "H".into(),
                row_span: 1,
                col_span: 2,
            }],
            vec![SpanCell::plain("x"), SpanCell::plain("y")],
        ]);
        assert_eq!((g[0][0].col_span, g[0][0].merged), (2, false));
        assert_eq!(g[0][0].text, "H");
        assert!(g[0][1].merged, "colspan-covered position");
        assert_eq!(g[0][1].text, "H", "covered text is replicated");
        assert_eq!(g[1][0].text, "x");
    }

    // TC-03: rowspan anchor + replicated covered position below it.
    #[test]
    fn expand_rowspan_materializes_covered() {
        let g = expand_spans(vec![
            vec![
                SpanCell {
                    text: "R".into(),
                    row_span: 2,
                    col_span: 1,
                },
                SpanCell::plain("b"),
            ],
            vec![SpanCell::plain("c")],
        ]);
        assert_eq!((g[0][0].row_span, g[0][0].merged), (2, false));
        assert!(g[1][0].merged, "rowspan-covered position");
        assert_eq!(g[1][0].text, "R", "covered text replicated downward");
        assert_eq!(g[1][1].text, "c", "the row's own cell flows past the cover");
    }

    #[test]
    fn table_spanned_carries_spans_and_widens_anchor_bbox() {
        let mut b = PageBuilder::letter();
        b.table_spanned(
            vec![
                vec![SpanCell {
                    text: "H".into(),
                    row_span: 1,
                    col_span: 2,
                }],
                vec![SpanCell::plain("x"), SpanCell::plain("y")],
            ],
            12.0,
        );
        let pages = b.finish();
        let table = pages
            .iter()
            .flat_map(|p| &p.elements)
            .find_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(table.rows[0][0].col_span, 2);
        assert!(!table.rows[0][0].merged);
        assert!(table.rows[0][1].merged);
        let anchor_w = table.rows[0][0].bbox.x1 - table.rows[0][0].bbox.x0;
        let plain_w = table.rows[1][0].bbox.x1 - table.rows[1][0].bbox.x0;
        assert!(anchor_w > plain_w, "anchor bbox spans two columns");
    }
}
