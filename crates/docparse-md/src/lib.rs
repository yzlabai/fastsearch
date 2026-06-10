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

    let flush = |b: &mut PageBuilder, buf: &mut String, size: f32| {
        let t = buf.trim();
        if !t.is_empty() {
            b.paragraph(t, size);
        }
        buf.clear();
    };

    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut b, &mut buf, size);
                size = heading_size(level);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut b, &mut buf, size);
                size = 11.0;
            }
            Event::Start(Tag::Paragraph) | Event::Start(Tag::Item) => flush(&mut b, &mut buf, size),
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Item) => {
                flush(&mut b, &mut buf, size)
            }
            Event::Start(Tag::CodeBlock(_)) => flush(&mut b, &mut buf, size),
            Event::End(TagEnd::CodeBlock) => flush(&mut b, &mut buf, size),
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
    flush(&mut b, &mut buf, size);

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
}
