//! HTML backend: parse a document with `scraper` (html5ever) and flow its
//! block structure onto a synthetic page (see `docparse_core::synth`).
//!
//! HTML carries explicit structure (headings, paragraphs, lists, tables) but no
//! coordinates, so we fabricate PDF-convention geometry and let the shared
//! reading-order/output layers handle the rest. Inline formatting is flattened
//! to text; scripts/styles are dropped.

use docparse_core::ir::Document;
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::Html;
use std::path::Path;

pub struct HtmlParser;

impl DocumentParser for HtmlParser {
    fn name(&self) -> &'static str {
        "html"
    }

    fn supports(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("html") | Some("htm") | Some("xhtml")
        )
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        // Decode honoring <meta charset> (H7) — `read_to_string` would reject
        // any non-UTF-8 page outright (legacy GBK/Shift-JIS/Windows-1252).
        let content = docparse_core::textio::decode_html(&std::fs::read(path)?);
        let mut doc = parse_str(&content);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse an HTML string into a [`Document`] (source left as "<html>").
pub fn parse_str(html: &str) -> Document {
    let dom = Html::parse_document(html);
    let mut b = PageBuilder::letter();
    walk(dom.tree.root(), &mut b);
    Document {
        source: "<html>".to_string(),
        provenance: Some(docparse_core::ir::Provenance::new(
            "html",
            env!("CARGO_PKG_VERSION"),
        )),
        pages: b.finish(),
    }
}

/// Heading font size by tag (body text is 12; larger ⇒ heading downstream).
fn heading_size(tag: &str) -> Option<f32> {
    Some(match tag {
        "h1" => 24.0,
        "h2" => 20.0,
        "h3" => 17.0,
        "h4" => 15.0,
        "h5" => 13.0,
        "h6" => 12.5,
        _ => return None,
    })
}

fn walk(node: NodeRef<Node>, b: &mut PageBuilder) {
    for child in node.children() {
        let Node::Element(el) = child.value() else {
            continue;
        };
        let tag = el.name();
        if let Some(size) = heading_size(tag) {
            b.paragraph(collect_text(child), size);
            continue;
        }
        match tag {
            "script" | "style" | "head" | "noscript" | "title" | "svg" => {}
            "p" | "blockquote" | "figcaption" | "pre" | "dd" | "dt" | "caption" => {
                b.paragraph(collect_text(child), 12.0);
            }
            "li" => b.paragraph(format!("- {}", collect_text(child)), 12.0),
            "table" => b.table(parse_table(child), 12.0),
            // Containers: recurse to preserve document order.
            _ => walk(child, b),
        }
    }
}

/// Concatenate all descendant text, collapsing runs of whitespace.
fn collect_text(node: NodeRef<Node>) -> String {
    let mut s = String::new();
    for d in node.descendants() {
        if let Node::Text(t) = d.value() {
            s.push_str(&t.text);
        }
    }
    collapse_ws(&s)
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build row-major cell text from a `<table>`: each descendant `tr` is a row,
/// each `td`/`th` a cell. Nested tables are flattened into their cell text.
fn parse_table(table: NodeRef<Node>) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for d in table.descendants() {
        let Node::Element(el) = d.value() else {
            continue;
        };
        if el.name() != "tr" {
            continue;
        }
        let mut row = Vec::new();
        for cell in d.children() {
            if let Node::Element(c) = cell.value() {
                if matches!(c.name(), "td" | "th") {
                    row.push(collect_text(cell));
                }
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::Element;

    #[test]
    fn headings_and_paragraphs() {
        let doc = parse_str("<html><body><h1>Title</h1><p>Hello world</p></body></html>");
        let texts: Vec<&str> = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Title", "Hello world"]);
    }

    #[test]
    fn table_parsed_into_table_element() {
        let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let doc = parse_str(html);
        let tables: Vec<_> = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 2);
        assert_eq!(tables[0].rows[0][0].text, "A");
        assert_eq!(tables[0].rows[1][1].text, "2");
    }

    #[test]
    fn scripts_and_styles_dropped() {
        let doc = parse_str("<body><script>var x=1</script><style>p{}</style><p>kept</p></body>");
        let n_text: usize = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter(|e| matches!(e, Element::Text(_)))
            .count();
        assert_eq!(n_text, 1);
    }
}
