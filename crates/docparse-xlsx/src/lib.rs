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
        // calamine's Display writes the raw Excel serial (e.g. "46190"); convert
        // date cells to ISO via chrono (correctly handles the 1900 leap quirk).
        // Durations have no calendar meaning — keep their numeric value.
        Data::DateTime(dt) => {
            if dt.is_datetime() {
                match dt.as_datetime() {
                    // NaiveDateTime Display is "YYYY-MM-DD HH:MM:SS"; drop a
                    // midnight time so plain dates read as just "YYYY-MM-DD".
                    Some(ndt) => {
                        let s = ndt.to_string();
                        s.strip_suffix(" 00:00:00").map(str::to_string).unwrap_or(s)
                    }
                    None => dt.as_f64().to_string(),
                }
            } else {
                dt.as_f64().to_string()
            }
        }
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

    #[test]
    fn integer_valued_floats_drop_the_decimal() {
        // Whole numbers render without a trailing ".0"; negatives too.
        assert_eq!(cell_text(&Data::Float(-7.0)), "-7");
        assert_eq!(cell_text(&Data::Float(0.0)), "0");
    }

    #[test]
    fn huge_floats_keep_full_precision() {
        // Above the 1e15 guard we fall back to {f} rather than an i64 cast that
        // would overflow / lose magnitude.
        let s = cell_text(&Data::Float(1e16));
        assert!(!s.is_empty());
        assert!(
            s.contains("1") && (s.contains("e16") || s.contains("0000")),
            "{s}"
        );
    }

    #[test]
    fn int_bool_and_error_cells() {
        assert_eq!(cell_text(&Data::Int(5)), "5");
        assert_eq!(cell_text(&Data::Bool(true)), "true");
        assert_eq!(cell_text(&Data::Bool(false)), "false");
        // Errors are surfaced (with a leading '#'), never silently blanked.
        let e = cell_text(&Data::Error(calamine::CellErrorType::Div0));
        assert!(e.starts_with('#'), "{e}");
    }

    #[test]
    fn iso_datetime_passes_through_verbatim() {
        assert_eq!(
            cell_text(&Data::DateTimeIso("2026-06-17T00:00:00".into())),
            "2026-06-17T00:00:00"
        );
    }

    #[test]
    fn datetime_cells_render_as_iso_not_serial() {
        use calamine::{ExcelDateTime, ExcelDateTimeType};
        // Excel serial 1 = 1900-01-01. Previously this rendered as "1" (the raw
        // serial via Display); now it's the ISO date, midnight time dropped.
        let date = ExcelDateTime::new(1.0, ExcelDateTimeType::DateTime, false);
        assert_eq!(cell_text(&Data::DateTime(date)), "1900-01-01");
        // A half-day fraction keeps the time component.
        let dt = ExcelDateTime::new(1.5, ExcelDateTimeType::DateTime, false);
        assert_eq!(cell_text(&Data::DateTime(dt)), "1900-01-01 12:00:00");
        // Durations have no calendar meaning — keep the numeric value.
        let dur = ExcelDateTime::new(2.5, ExcelDateTimeType::TimeDelta, false);
        assert_eq!(cell_text(&Data::DateTime(dur)), "2.5");
    }
}
