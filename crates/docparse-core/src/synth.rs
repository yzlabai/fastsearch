//! Synthetic page layout for formats without real coordinates (DOCX, HTML).
//!
//! Backends for structured formats know the *logical* order of paragraphs,
//! headings and tables but have no PDF-style geometry. To keep the IR's single
//! coordinate convention (PDF user space, origin bottom-left, y-up, points), we
//! flow blocks top-to-bottom down a synthetic page with margins, paginating
//! when we run out of vertical space. Reading order and output then work
//! unchanged. Coordinates are an honest fabrication — useful for relative
//! ordering and citation anchoring, not for fidelity to any real page.

use crate::ir::{BBox, Cell, Element, Page, Table, TextChunk};

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

    /// Add a paragraph (or heading — just use a larger `size`). Empty text is
    /// skipped. One chunk per block; the output layer reconstructs paragraphs.
    pub fn paragraph(&mut self, text: impl Into<String>, size: f32) {
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
            tag: None,
        }));
        self.y -= lh;
    }

    /// Add a table from row-major cell text. Cells get synthetic grid bboxes so
    /// downstream sees a real [`Table`] element.
    pub fn table(&mut self, rows: Vec<Vec<String>>, size: f32) {
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 || rows.is_empty() {
            return;
        }
        let row_h = Self::line_height(size) + size * 0.4;
        let total = row_h * rows.len() as f32;
        self.ensure(total.min(self.height - 2.0 * self.margin));
        let col_w = (self.width - 2.0 * self.margin) / ncols as f32;
        let top = self.y;
        let mut out_rows: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
        for (r, row) in rows.iter().enumerate() {
            let y_top = top - r as f32 * row_h;
            let y_bot = y_top - row_h;
            let mut cells = Vec::with_capacity(ncols);
            for c in 0..ncols {
                let x0 = self.margin + c as f32 * col_w;
                let text = row.get(c).cloned().unwrap_or_default();
                cells.push(Cell {
                    text,
                    bbox: BBox {
                        x0,
                        y0: y_bot,
                        x1: x0 + col_w,
                        y1: y_top,
                    },
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
}
