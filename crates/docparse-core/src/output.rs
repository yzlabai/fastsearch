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

/// Tables detected on a page.
fn page_tables(page: &Page) -> Vec<&Table> {
    page.elements
        .iter()
        .filter_map(|e| match e {
            Element::Table(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// Per-page reconstruction: text blocks (table content excluded, headers/footers
/// dropped, paragraphs grouped) plus the page's tables.
struct PageContent<'a> {
    blocks: Vec<Block>,
    tables: Vec<&'a Table>,
}

fn document_content(doc: &Document) -> Vec<PageContent<'_>> {
    layout::page_blocks(doc)
        .into_iter()
        .zip(&doc.pages)
        .map(|(blocks, page)| PageContent { blocks, tables: page_tables(page) })
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
            if block.heading {
                md.push_str("## ");
            }
            md.push_str(t);
            md.push_str("\n\n");
        }
        for table in &pc.tables {
            md.push_str(&markdown_table(table));
            md.push('\n');
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
