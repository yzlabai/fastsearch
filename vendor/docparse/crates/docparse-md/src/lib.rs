//! Markdown backend: pulldown-cmark events → headings/paragraphs/lists/tables
//! on the synthetic layout. Code blocks keep their line breaks.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser as MdParser, Tag, TagEnd};
use std::path::Path;

pub struct MarkdownParser;

impl DocumentParser for MarkdownParser {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let text = std::fs::read_to_string(path)?;
        let mut doc = parse_str(&text);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

fn heading_size(level: HeadingLevel) -> f32 {
    match level {
        HeadingLevel::H1 => 20.0,
        HeadingLevel::H2 => 17.0,
        HeadingLevel::H3 => 15.0,
        _ => 13.0,
    }
}

/// Flush the inline buffer as one block. Inside a list (`lists` non-empty) and
/// when `as_list` is set, the block becomes a list item with a normalized
/// marker — `• ` for bullets, `N. ` for ordered lists (the counter lives in the
/// stack top and advances per item) — and the `LI` tag so downstream treats it
/// as a list, not a numbered heading. Heading/code content passes
/// `as_list = false` so it never picks up a bullet. Empty text is dropped.
fn flush(
    b: &mut PageBuilder,
    buf: &mut String,
    size: f32,
    lists: &mut [Option<u64>],
    as_list: bool,
) {
    let t = buf.trim();
    if !t.is_empty() {
        match lists.last_mut().filter(|_| as_list) {
            Some(slot) => {
                let marker = match slot {
                    Some(n) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    None => "• ".to_string(),
                };
                b.list_item(format!("{marker}{t}"), size);
            }
            None => b.paragraph(t, size),
        }
    }
    buf.clear();
}

/// Parse Markdown text into a [`Document`].
pub fn parse_str(text: &str) -> Document {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    let parser = MdParser::new_ext(text, opts);

    let mut b = PageBuilder::letter();
    let mut buf = String::new();
    let mut size = 11.0f32;
    // table state
    let mut table: Option<Vec<Vec<String>>> = None;
    let mut row: Vec<String> = Vec::new();
    // list nesting: each entry is the next ordinal for an ordered list, or
    // `None` for a bullet list. Nesting is flattened (markers normalized), like
    // the AsciiDoc/TeX backends.
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut b, &mut buf, size, &mut list_stack, true);
                size = heading_size(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                // Heading text is its own block, never a bullet.
                flush(&mut b, &mut buf, size, &mut list_stack, false);
                size = 11.0;
            }
            Event::Start(Tag::List(start)) => {
                // Any pending text belongs to the enclosing item (or a preceding
                // paragraph); emit it before opening the new list level.
                flush(&mut b, &mut buf, size, &mut list_stack, true);
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut b, &mut buf, size, &mut list_stack, true);
                list_stack.pop();
            }
            Event::Start(Tag::Paragraph) | Event::Start(Tag::Item) => {
                flush(&mut b, &mut buf, size, &mut list_stack, true)
            }
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Item) => {
                flush(&mut b, &mut buf, size, &mut list_stack, true)
            }
            Event::Start(Tag::CodeBlock(_)) => flush(&mut b, &mut buf, size, &mut list_stack, true),
            Event::End(TagEnd::CodeBlock) => {
                // Code content is verbatim, never a bullet.
                flush(&mut b, &mut buf, size, &mut list_stack, false)
            }
            Event::Start(Tag::Table(_)) => table = Some(Vec::new()),
            Event::End(TagEnd::Table) => {
                if let Some(rows) = table.take() {
                    if !rows.is_empty() {
                        b.table(rows, 10.0);
                    }
                }
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => row.clear(),
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                if let Some(t) = table.as_mut() {
                    t.push(std::mem::take(&mut row));
                }
            }
            Event::End(TagEnd::TableCell) => row.push(std::mem::take(&mut buf).trim().to_string()),
            Event::Text(t) | Event::Code(t) => buf.push_str(&t),
            Event::SoftBreak => buf.push(' '),
            Event::HardBreak => buf.push('\n'),
            _ => {}
        }
    }
    flush(&mut b, &mut buf, size, &mut list_stack, false);

    Document {
        source: "<markdown>".to_string(),
        provenance: Some(Provenance::new("markdown", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_str;
    use docparse_core::ir::Element;

    #[test]
    fn headings_paragraphs_and_tables() {
        let doc = parse_str("# Title\n\nBody text.\n\n|a|b|\n|-|-|\n|1|2|\n");
        let page = &doc.pages[0];
        let texts: Vec<&str> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"Title"));
        assert!(texts.contains(&"Body text."));
        let tables = page
            .elements
            .iter()
            .filter(|e| matches!(e, Element::Table(_)))
            .count();
        assert_eq!(tables, 1);
    }

    fn texts(doc: &docparse_core::ir::Document) -> Vec<(String, f32)> {
        doc.pages[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.font_size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn heading_levels_map_to_descending_sizes() {
        let doc = parse_str("# H1\n\n## H2\n\n### H3\n\n#### H4\n\nbody\n");
        let t = texts(&doc);
        let size = |label: &str| t.iter().find(|(s, _)| s == label).unwrap().1;
        // H1 > H2 > H3, H4+ collapses to the same fallback size, body is body.
        assert!(size("H1") > size("H2"));
        assert!(size("H2") > size("H3"));
        assert!(size("H3") > size("H4"));
        assert_eq!(size("H4"), 13.0);
        assert_eq!(size("body"), 11.0);
    }

    #[test]
    fn code_block_keeps_line_breaks() {
        let doc = parse_str("```\nline1\nline2\n```\n");
        let t = texts(&doc);
        assert!(
            t.iter().any(|(s, _)| s.contains("line1\nline2")),
            "code block should preserve internal newlines: {t:?}"
        );
    }

    #[test]
    fn soft_break_becomes_space() {
        // A single newline inside a paragraph is a soft break → joined by space.
        let doc = parse_str("first\nsecond\n");
        let t = texts(&doc);
        assert!(t.iter().any(|(s, _)| s == "first second"), "{t:?}");
    }

    #[test]
    fn empty_input_produces_no_text() {
        let doc = parse_str("");
        assert!(texts(&doc).is_empty());
    }

    fn text_tags(doc: &docparse_core::ir::Document) -> Vec<(String, Option<String>)> {
        doc.pages[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.tag.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bullet_list_items_get_marker_and_li_tag() {
        // Previously list items flattened to bare paragraphs (no marker, no tag),
        // so downstream demoted them to prose. Now they carry the • marker and
        // the LI tag like the AsciiDoc/TeX/HTML backends.
        let doc = parse_str("- alpha\n- beta\n");
        assert_eq!(
            text_tags(&doc),
            vec![
                ("• alpha".to_string(), Some("LI".to_string())),
                ("• beta".to_string(), Some("LI".to_string())),
            ]
        );
    }

    #[test]
    fn ordered_list_items_are_numbered_from_their_start() {
        // The marker numbering honors the list's start value (pulldown reports
        // the first ordinal), so "3." leads.
        let doc = parse_str("3. three\n4. four\n");
        let t: Vec<String> = text_tags(&doc).into_iter().map(|(s, _)| s).collect();
        assert_eq!(t, vec!["3. three", "4. four"]);
    }

    #[test]
    fn nested_list_flattens_keeping_each_levels_marker() {
        // A bullet item with a nested ordered list: nesting is flattened, but
        // each item keeps the marker of its own list.
        let doc = parse_str("- a\n    1. x\n    2. y\n- b\n");
        let t: Vec<String> = text_tags(&doc).into_iter().map(|(s, _)| s).collect();
        assert_eq!(t, vec!["• a", "1. x", "2. y", "• b"]);
    }

    #[test]
    fn code_block_inside_list_is_not_bulleted() {
        // The item's lead text is a bullet; the fenced code below it stays
        // verbatim (no marker) rather than becoming a bullet line.
        let doc = parse_str("- item\n\n  ```\n  code\n  ```\n");
        let t: Vec<String> = text_tags(&doc).into_iter().map(|(s, _)| s).collect();
        assert!(t.iter().any(|s| s == "• item"), "{t:?}");
        assert!(t.iter().any(|s| s == "code"), "{t:?}");
        assert!(
            !t.iter().any(|s| s == "• code"),
            "code must not be bulleted: {t:?}"
        );
    }
}
