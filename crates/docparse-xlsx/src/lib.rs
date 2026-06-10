//! XLSX backend: each worksheet becomes a heading (sheet name) plus one table
//! on the shared synthetic layout — same IR, same outputs as every backend.

use anyhow::Context;
use calamine::{Data, Reader, Xlsx};
use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use std::io::Cursor;
use std::path::Path;

pub struct XlsxParser;

impl DocumentParser for XlsxParser {
    fn name(&self) -> &'static str {
        "xlsx"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xlsx"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let buf = std::fs::read(path)?;
        let mut doc = parse_bytes(&buf)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse XLSX bytes into a [`Document`].
pub fn parse_bytes(buf: &[u8]) -> anyhow::Result<Document> {
    docparse_core::limits::check_zip_bomb(buf)?;
    let mut wb: Xlsx<_> = Xlsx::new(Cursor::new(buf)).context("xlsx open")?;
    let mut b = PageBuilder::letter();
    let names: Vec<String> = wb.sheet_names().to_vec();
    for name in names {
        let Ok(range) = wb.worksheet_range(&name) else {
            continue;
        };
        let rows: Vec<Vec<String>> = range
            .rows()
            .map(|r| r.iter().map(cell_text).collect())
            .filter(|r: &Vec<String>| r.iter().any(|c| !c.is_empty()))
            .collect();
        if rows.is_empty() {
            continue;
        }
        b.paragraph(&name, 16.0); // sheet name as a heading-sized line
        b.table(rows, 10.0);
        b.page_break(); // one sheet per page
    }
    Ok(Document {
        source: "<xlsx>".to_string(),
        provenance: Some(Provenance::new("xlsx", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    })
}

/// Render a cell as display text (formulas come through as cached values).
fn cell_text(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => format!("{i}"),
        Data::Bool(v) => format!("{v}"),
        Data::DateTime(dt) => format!("{dt}"),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#{e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::cell_text;
    use calamine::Data;

    #[test]
    fn cells_render_as_display_text() {
        assert_eq!(cell_text(&Data::Float(42.0)), "42");
        assert_eq!(cell_text(&Data::Float(3.5)), "3.5");
        assert_eq!(cell_text(&Data::String("x".into())), "x");
        assert_eq!(cell_text(&Data::Empty), "");
    }
}
