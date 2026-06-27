//! AsciiDoc backend (G1b long-tail formats): the common article subset,
//! hand-rolled and line-oriented — no dependency, no full AsciiDoc engine.
//!
//! Scope (documented bounds):
//! - `= Title` document title, `==`…`=====` section headings;
//! - paragraphs split on blank lines; `//` comment lines dropped;
//! - `:attr: value` attribute lines and `[..]` block-style lines skipped;
//! - lists: `*`/`-` bullets and `.` ordinals (nesting flattened; markers
//!   normalized so the core list channel applies);
//! - delimited blocks: `----`/`....` → code (verbatim lines), `____` quote
//!   (as paragraphs), `|===` tables (one `|`-prefixed line per row);
//! - inline formatting passes through as-is (bold `*x*` etc. are readable
//!   plain text; stripping them loses more than it gains).
//!
//! NOT handled: includes (one file = one document), conditionals, macros,
//! multi-line cell tables. Exotic input degrades to plain paragraphs.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use std::path::Path;

pub struct AdocParser;

impl DocumentParser for AdocParser {
    fn name(&self) -> &'static str {
        "asciidoc"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("adoc") || e.eq_ignore_ascii_case("asciidoc"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let text = docparse_core::textio::read_text(path)?;
        let mut doc = parse_str(&text);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

const BODY_SIZE: f32 = 10.0;
const TITLE_SIZE: f32 = 18.0;
/// `==`..`=====` heading sizes (all > 1.25 × body for the core heading rule).
const HEADING_SIZES: [f32; 4] = [16.0, 14.0, 12.6, 12.6];

/// Parse AsciiDoc text into a [`Document`].
pub fn parse_str(src: &str) -> Document {
    let mut b = PageBuilder::letter();
    let mut para = String::new();
    let mut code: Option<String> = None; // open delimiter
    let mut table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut ordinal = 0usize;

    let flush = |b: &mut PageBuilder, buf: &mut String| {
        let t = buf.trim();
        if !t.is_empty() {
            b.paragraph(t.to_string(), BODY_SIZE);
        }
        buf.clear();
    };

    for line in src.lines() {
        let t = line.trim_end();

        if let Some(delim) = &code {
            if t.trim() == delim {
                code = None;
            } else {
                b.paragraph(t.to_string(), BODY_SIZE);
            }
            continue;
        }
        if table {
            if t.trim() == "|===" {
                if !table_rows.is_empty() {
                    b.table(std::mem::take(&mut table_rows), BODY_SIZE);
                }
                table = false;
            } else if let Some(rest) = t.trim().strip_prefix('|') {
                table_rows.push(rest.split('|').map(|c| c.trim().to_string()).collect());
            }
            continue;
        }

        let trimmed = t.trim();
        if trimmed.is_empty() {
            flush(&mut b, &mut para);
            ordinal = 0;
            continue;
        }
        if trimmed.starts_with("//") || trimmed.starts_with(':') && trimmed[1..].contains(':') {
            continue; // comment / attribute line
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            continue; // block style/attributes
        }
        if trimmed == "----" || trimmed == "...." {
            flush(&mut b, &mut para);
            code = Some(trimmed.to_string());
            continue;
        }
        if trimmed == "|===" {
            flush(&mut b, &mut para);
            table = true;
            continue;
        }
        if trimmed == "____" {
            flush(&mut b, &mut para);
            continue; // quote delimiters drop; quoted text reads as paragraphs
        }
        // Headings: "= Title", "== Section", ...
        if let Some((eqs, rest)) = split_marker(trimmed, '=') {
            flush(&mut b, &mut para);
            let size = match eqs {
                1 => TITLE_SIZE,
                n => HEADING_SIZES[(n - 2).min(HEADING_SIZES.len() - 1)],
            };
            b.paragraph(rest.to_string(), size);
            continue;
        }
        // Lists: "* x" / "- x" bullets, ". x" ordinals (nesting flattened).
        if let Some((_, rest)) = split_marker(trimmed, '*').or_else(|| split_marker(trimmed, '-')) {
            flush(&mut b, &mut para);
            b.list_item(format!("• {rest}"), BODY_SIZE);
            continue;
        }
        if let Some((_, rest)) = split_marker(trimmed, '.') {
            flush(&mut b, &mut para);
            ordinal += 1;
            b.list_item(format!("{ordinal}. {rest}"), BODY_SIZE);
            continue;
        }
        para.push_str(trimmed);
        para.push(' ');
    }
    flush(&mut b, &mut para);
    if !table_rows.is_empty() {
        b.table(table_rows, BODY_SIZE);
    }

    Document {
        source: "<asciidoc>".to_string(),
        provenance: Some(Provenance::new("asciidoc", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    }
}

/// `"== Heading"` → `(2, "Heading")`: a run of `marker` followed by a space.
fn split_marker(line: &str, marker: char) -> Option<(usize, &str)> {
    let n = line.chars().take_while(|&c| c == marker).count();
    if n == 0 {
        return None;
    }
    let rest = &line[n..];
    let rest = rest.strip_prefix(' ')?;
    (!rest.is_empty()).then_some((n, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::Element;

    fn texts(doc: &Document) -> Vec<(String, f32)> {
        doc.pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.font_size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn title_sections_lists_paragraphs() {
        let doc = parse_str(
            "= My Doc\n:toc: left\n\n== Intro\n\nBody text\ncontinues here.\n\n* alpha\n* beta\n\n. one\n. two\n\n// comment\n",
        );
        let t = texts(&doc);
        assert_eq!(t[0], ("My Doc".to_string(), TITLE_SIZE));
        assert_eq!(t[1], ("Intro".to_string(), HEADING_SIZES[0]));
        assert_eq!(t[2].0, "Body text continues here.");
        assert_eq!(t[3].0, "• alpha");
        assert_eq!(t[5].0, "1. one");
        assert_eq!(t[6].0, "2. two");
    }

    #[test]
    fn code_block_and_table() {
        let doc =
            parse_str("before\n\n----\nfn main() {}\n----\n\n|===\n| H1 | H2\n| a | b\n|===\n");
        let t = texts(&doc);
        assert!(t.iter().any(|(s, _)| s == "fn main() {}"));
        let tables: Vec<_> = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Table(tb) => Some(tb),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows[0][0].text, "H1");
        assert_eq!(tables[0].rows[1][1].text, "b");
    }

    #[test]
    fn dotted_prose_is_not_an_ordinal() {
        // ". x" is a list item only at line start with the marker run; a
        // sentence ending in "." stays prose.
        let doc = parse_str("Plain sentence.\nNext line.\n");
        let t = texts(&doc);
        assert_eq!(t.len(), 1);
        assert!(t[0].0.starts_with("Plain sentence."));
    }

    #[test]
    fn split_marker_requires_run_then_space() {
        assert_eq!(split_marker("== Heading", '='), Some((2, "Heading")));
        assert_eq!(split_marker("* item", '*'), Some((1, "item")));
        // A marker run with no following space is not a marker (e.g. inline
        // bold `*bold*` or a bare `===` rule line).
        assert_eq!(split_marker("*bold*", '*'), None);
        assert_eq!(split_marker("===", '='), None);
        assert_eq!(split_marker("text", '='), None);
    }

    #[test]
    fn quote_and_literal_blocks() {
        // ____ quote delimiters drop; inner text reads as a paragraph. ....
        // literal keeps its lines verbatim.
        let doc = parse_str("____\nquoted text\n____\n\n....\nverbatim line\n....\n");
        let t = texts(&doc);
        let strs: Vec<&str> = t.iter().map(|(s, _)| s.as_str()).collect();
        assert!(strs.contains(&"quoted text"));
        assert!(strs.contains(&"verbatim line"));
        // The delimiters themselves never surface as text.
        assert!(t.iter().all(|(s, _)| s != "____" && s != "...."));
    }

    #[test]
    fn block_attribute_lines_are_skipped() {
        let doc = parse_str("[source,rust]\nfn main() {}\n");
        let t = texts(&doc);
        assert!(t.iter().all(|(s, _)| !s.starts_with('[')));
        assert!(t.iter().any(|(s, _)| s == "fn main() {}"));
    }

    #[test]
    fn ordinals_reset_between_lists() {
        // A blank line ends the list; the next ordinal run restarts at 1.
        let doc = parse_str(". one\n. two\n\n. fresh\n");
        let t: Vec<String> = texts(&doc).into_iter().map(|(s, _)| s).collect();
        assert_eq!(t, vec!["1. one", "2. two", "1. fresh"]);
    }

    #[test]
    fn deep_headings_clamp_to_last_size() {
        let doc = parse_str("===== five\n");
        let (_, size) = texts(&doc).into_iter().next().unwrap();
        assert_eq!(size, HEADING_SIZES[HEADING_SIZES.len() - 1]);
    }

    #[test]
    fn unclosed_table_flushes_at_eof() {
        let doc = parse_str("|===\n| a | b\n");
        let tables: Vec<_> = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Table(tb) => Some(tb),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows[0][0].text, "a");
        assert_eq!(tables[0].rows[0][1].text, "b");
    }
}
