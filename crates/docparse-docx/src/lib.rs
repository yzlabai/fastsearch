//! DOCX backend: read Office Open XML with `docx-rs` and flow its paragraphs,
//! headings and tables onto a synthetic page (see `docparse_core::synth`).
//!
//! DOCX has explicit structure (paragraph styles, table grids) but no
//! coordinates, so geometry is fabricated under the PDF convention and the
//! shared reading-order/output layers take over. Heading levels come from the
//! paragraph style name ("Heading1" …); tables map straight to `Table`.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use docx_rs::{
    DocumentChild, Paragraph, ParagraphChild, RunChild, Table, TableCellContent, TableChild,
    TableRowChild,
};
use std::path::Path;

pub struct DocxParser;

impl DocumentParser for DocxParser {
    fn name(&self) -> &'static str {
        "docx"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("docx"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let buf = std::fs::read(path)?;
        let mut doc = parse_bytes(&buf)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse DOCX bytes into a [`Document`].
pub fn parse_bytes(buf: &[u8]) -> anyhow::Result<Document> {
    let docx = docx_rs::read_docx(buf).map_err(|e| anyhow::anyhow!("docx parse: {e}"))?;
    let mut b = PageBuilder::letter();

    for child in &docx.document.children {
        match child {
            DocumentChild::Paragraph(p) => {
                b.paragraph(paragraph_text(p), paragraph_size(p));
            }
            DocumentChild::Table(t) => {
                b.table(table_rows(t), 12.0);
            }
            _ => {}
        }
    }

    Ok(Document {
        source: "<docx>".to_string(),
        provenance: Some(Provenance::new("docx", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    })
}

/// Concatenate a paragraph's run text.
fn paragraph_text(p: &Paragraph) -> String {
    let mut s = String::new();
    for child in &p.children {
        if let ParagraphChild::Run(run) = child {
            for rc in &run.children {
                match rc {
                    RunChild::Text(t) => s.push_str(&t.text),
                    RunChild::Tab(_) => s.push('\t'),
                    _ => {}
                }
            }
        }
    }
    s
}

/// Font size from the paragraph style name ("Heading1" …, "Title").
fn paragraph_size(p: &Paragraph) -> f32 {
    let style = p.property.style.as_ref().map(|s| s.val.as_str()).unwrap_or("");
    let lower = style.to_ascii_lowercase();
    if lower == "title" {
        return 26.0;
    }
    match lower.strip_prefix("heading").and_then(|n| n.trim().parse::<u32>().ok()) {
        Some(1) => 24.0,
        Some(2) => 20.0,
        Some(3) => 17.0,
        Some(4) => 15.0,
        Some(_) => 13.0,
        None => 12.0,
    }
}

/// Row-major cell text from a DOCX table.
fn table_rows(t: &Table) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for TableChild::TableRow(row) in &t.rows {
        let mut r = Vec::new();
        for TableRowChild::TableCell(cell) in &row.cells {
            let mut text = String::new();
            for content in &cell.children {
                if let TableCellContent::Paragraph(p) = content {
                    let t = paragraph_text(p);
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(&t);
                    }
                }
            }
            r.push(text);
        }
        if !r.is_empty() {
            rows.push(r);
        }
    }
    rows
}
