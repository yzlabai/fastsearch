//! Output serializers: JSON (full IR), Markdown, and plain text.
//!
//! Markdown/text are built from [`crate::layout`] blocks: per-glyph chunks are
//! rebuilt into lines (word spaces by geometric gap), text inside a detected
//! table is excluded, running headers/footers dropped, and consecutive lines
//! grouped into paragraphs/headings. Tables render as their own blocks.

use crate::ir::{Document, Element, Page, Table};
use crate::layout::{self, Block};

/// Full IR as pretty JSON.
pub fn to_json(doc: &Document) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(doc)?)
}

/// Tables detected on a page. Empty-row tables (e.g. an unfilled layout-seeded
/// table-region placeholder) are skipped — a table with no cells isn't content.
fn page_tables(page: &Page) -> Vec<&Table> {
    page.elements
        .iter()
        .filter_map(|e| match e {
            Element::Table(t) if !t.rows.is_empty() => Some(t),
            _ => None,
        })
        .collect()
}

/// Per-page reconstruction: text blocks (table content excluded, headers/footers
/// dropped, paragraphs grouped) plus the page's tables and renderable images.
struct PageContent<'a> {
    blocks: Vec<Block>,
    tables: Vec<&'a Table>,
    /// Images worth rendering: exported to disk (`file` set, referenced in
    /// Markdown) or carrying a caption (a VLM description to surface).
    images: Vec<&'a crate::ir::ImageChunk>,
}

fn document_content(doc: &Document) -> Vec<PageContent<'_>> {
    layout::page_blocks(doc)
        .into_iter()
        .zip(&doc.pages)
        .map(|(blocks, page)| PageContent {
            blocks,
            tables: page_tables(page),
            images: page
                .elements
                .iter()
                .filter_map(|e| match e {
                    Element::Image(i) if i.file.is_some() || i.caption.is_some() => Some(i),
                    _ => None,
                })
                .collect(),
        })
        .collect()
}

/// Plain text: paragraphs one per line; tables as tab-separated rows.
pub fn to_text(doc: &Document) -> String {
    let mut s = String::new();
    for pc in document_content(doc) {
        for block in &pc.blocks {
            s.push_str(block.text.trim());
            s.push('\n');
        }
        for table in &pc.tables {
            for row in &table.rows {
                let cells: Vec<&str> = row.iter().map(|c| c.text.trim()).collect();
                s.push_str(&cells.join("\t"));
                s.push('\n');
            }
            s.push('\n');
        }
        for img in &pc.images {
            if let Some(c) = &img.caption {
                s.push_str(c.trim());
                s.push('\n');
            }
        }
        s.push('\n');
    }
    s
}

/// Markdown: blocks become paragraphs (`##` for headings); tables become pipe
/// tables (first row treated as the header).
pub fn to_markdown(doc: &Document) -> String {
    let mut md = format!("<!-- source: {} -->\n\n", doc.source);
    for pc in document_content(doc) {
        for block in &pc.blocks {
            let t = block.text.trim();
            if t.is_empty() {
                continue;
            }
            if block.code {
                md.push_str("```\n");
                md.push_str(&block.text);
                md.push_str("\n```\n\n");
                continue;
            }
            if block.list_item {
                // Bullets normalize to "-"; ordinals keep their own numbering
                // (Markdown renders both as lists).
                let t = block.text.trim_start();
                let rendered = match t.chars().next() {
                    Some('•' | '·' | '‣' | '▪' | '◦' | '○' | '–') => {
                        format!(
                            "- {}",
                            t[t.chars().next().unwrap().len_utf8()..].trim_start()
                        )
                    }
                    _ => t.to_string(),
                };
                md.push_str(&rendered);
                md.push('\n');
                continue;
            }
            if block.heading {
                // Level 1 → "## " (single # reserved for a document title),
                // deeper levels nest accordingly.
                for _ in 0..(block.level.clamp(1, 4) + 1) {
                    md.push('#');
                }
                md.push(' ');
            }
            md.push_str(t);
            md.push_str("\n\n");
        }
        for table in &pc.tables {
            md.push_str(&markdown_table(table));
            md.push('\n');
        }
        for img in &pc.images {
            // Caption (e.g. a VLM description) becomes the image's alt text;
            // a caption-only image (no exported file) still surfaces its text.
            let alt = img
                .caption
                .as_deref()
                .map(|c| c.replace(['\n', '\r'], " ").replace(']', ")"))
                .unwrap_or_else(|| format!("image p{}", img.page));
            match &img.file {
                Some(f) => md.push_str(&format!("![{alt}]({f})\n\n")),
                None => {
                    if img.caption.is_some() {
                        md.push_str(&format!("*{}*\n\n", alt.trim()));
                    }
                }
            }
        }
    }
    md
}

/// Render a table as a GitHub-flavored Markdown pipe table.
fn markdown_table(table: &Table) -> String {
    let mut s = String::new();
    let cols = table.rows.first().map(|r| r.len()).unwrap_or(0);
    if cols == 0 {
        return s;
    }
    let esc = |t: &str| t.replace('|', "\\|").replace('\n', " ");
    for (r, row) in table.rows.iter().enumerate() {
        s.push('|');
        for cell in row {
            s.push(' ');
            s.push_str(esc(cell.text.trim()).trim());
            s.push_str(" |");
        }
        s.push('\n');
        if r == 0 {
            s.push('|');
            for _ in 0..cols {
                s.push_str(" --- |");
            }
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BBox, ImageChunk, ImageKind, Page};

    fn img(file: Option<&str>, caption: Option<&str>) -> Element {
        Element::Image(ImageChunk {
            bbox: BBox {
                x0: 72.0,
                y0: 400.0,
                x1: 500.0,
                y1: 700.0,
            },
            page: 1,
            width_px: 100,
            height_px: 100,
            turns: 0,
            kind: ImageKind::None,
            data: Vec::new(),
            file: file.map(Into::into),
            data_base64: None,
            data_media_type: None,
            caption: caption.map(Into::into),
            caption_source: caption.map(|_| "vlm:test".into()),
        })
    }

    fn doc(elements: Vec<Element>) -> Document {
        Document {
            source: "t".into(),
            provenance: None,
            pages: vec![Page {
                number: 1,
                width: 612.0,
                height: 792.0,
                elements,
            }],
        }
    }

    #[test]
    fn markdown_uses_caption_as_alt_text() {
        let md = to_markdown(&doc(vec![img(
            Some("assets/fig.png"),
            Some("A bar chart of revenue."),
        )]));
        assert!(
            md.contains("![A bar chart of revenue.](assets/fig.png)"),
            "got: {md}"
        );
    }

    #[test]
    fn caption_only_image_renders_as_italic_line() {
        // No exported file, but a VLM caption — surface it rather than drop it.
        let md = to_markdown(&doc(vec![img(None, Some("A flow diagram."))]));
        assert!(md.contains("*A flow diagram.*"), "got: {md}");
        let txt = to_text(&doc(vec![img(None, Some("A flow diagram."))]));
        assert!(txt.contains("A flow diagram."), "got: {txt}");
    }

    #[test]
    fn image_without_file_or_caption_is_not_rendered() {
        let md = to_markdown(&doc(vec![img(None, None)]));
        assert!(!md.contains("!["), "no image syntax: {md}");
    }
}
