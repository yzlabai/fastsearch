//! CSV backend: the whole file becomes one table on the synthetic layout.
//! Hand-rolled RFC-4180-ish parser (quotes, embedded commas/newlines) — no
//! dependency needed for a format this small.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use std::path::Path;

pub struct CsvParser;

impl DocumentParser for CsvParser {
    fn name(&self) -> &'static str {
        "csv"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("csv"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let text = docparse_core::textio::read_text(path)?;
        let mut doc = parse_str(&text);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse CSV text into a [`Document`].
pub fn parse_str(text: &str) -> Document {
    let rows = parse_rows(text);
    let mut b = PageBuilder::letter();
    if !rows.is_empty() {
        b.table(rows, 10.0);
    }
    Document {
        source: "<csv>".to_string(),
        provenance: Some(Provenance::new("csv", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    }
}

fn parse_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            }
            (true, c) => field.push(c),
            (false, '"') if field.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {} // swallow; \n terminates
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|f| !f.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        if row.iter().any(|f| !f.trim().is_empty()) {
            rows.push(row);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::parse_rows;

    #[test]
    fn quotes_commas_and_newlines() {
        let rows = parse_rows("a,b,c\n\"x,1\",\"say \"\"hi\"\"\",\"two\nlines\"\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a", "b", "c"]);
        assert_eq!(rows[1], vec!["x,1", "say \"hi\"", "two\nlines"]);
    }

    #[test]
    fn empty_input_has_no_rows() {
        assert!(parse_rows("").is_empty());
        // Whitespace-only / blank lines are dropped, not kept as empty rows.
        assert!(parse_rows("\n\n  \n").is_empty());
    }

    #[test]
    fn final_row_without_trailing_newline() {
        // The loop terminator must flush the last field even with no closing \n.
        let rows = parse_rows("a,b\nc,d");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn crlf_line_endings() {
        // \r is swallowed; \n terminates. Output must not carry stray \r.
        let rows = parse_rows("a,b\r\nc,d\r\n");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn blank_lines_between_records_are_skipped() {
        let rows = parse_rows("a,b\n\nc,d\n");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn trailing_empty_fields_are_preserved() {
        // A blank cell at the end of a row is content-bearing structurally as
        // long as the row has any non-empty field, and must be kept.
        let rows = parse_rows("a,,c\n");
        assert_eq!(rows, vec![vec!["a", "", "c"]]);
    }

    #[test]
    fn quote_only_opens_at_field_start() {
        // A `"` mid-field is literal (the opening-quote guard requires an empty
        // field), so embedded quotes in unquoted fields pass through verbatim.
        let rows = parse_rows("ab\"c\"d,e\n");
        assert_eq!(rows, vec![vec!["ab\"c\"d", "e"]]);
    }
}
