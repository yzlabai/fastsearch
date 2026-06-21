//! HTML backend: parse a document with `scraper` (html5ever) and flow its
//! block structure onto a synthetic page (see `docparse_core::synth`).
//!
//! HTML carries explicit structure (headings, paragraphs, lists, tables) but no
//! coordinates, so we fabricate PDF-convention geometry and let the shared
//! reading-order/output layers handle the rest. Inline formatting is flattened
//! to text; scripts/styles are dropped.
//!
//! `<img>` images are flowed as `ImageChunk`s with the `alt` text as their
//! caption (the searchable field for RAG). Bytes come from `data:` URIs
//! (decoded) or from a relative path resolved against the HTML file's directory;
//! remote `http(s)://` images are not fetched (skipped).

use base64::Engine;
use docparse_core::ir::Document;
use docparse_core::parser::DocumentParser;
use docparse_core::synth::{image_mime_from_path, PageBuilder, SpanCell};
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
        // The file's directory resolves relative <img src> paths.
        let mut doc = parse_html(&content, path.parent());
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse an HTML string into a [`Document`] (source left as "<html>"). Relative
/// `<img>` paths can't be resolved without a base directory, so only `data:`
/// URIs yield image bytes here; use [`HtmlParser::parse`] for file-relative
/// images.
pub fn parse_str(html: &str) -> Document {
    parse_html(html, None)
}

/// Parse HTML, resolving relative `<img>` paths against `base` (the HTML file's
/// directory) when given.
fn parse_html(html: &str, base: Option<&Path>) -> Document {
    let dom = Html::parse_document(html);
    let mut b = PageBuilder::letter();
    walk(dom.tree.root(), &mut b, base);
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

fn walk(node: NodeRef<Node>, b: &mut PageBuilder, base: Option<&Path>) {
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
            "ul" => walk_list(child, b, None),
            "ol" => {
                // Honor <ol start="N">; default to 1.
                let start = el.attr("start").and_then(|s| s.parse().ok()).unwrap_or(1);
                walk_list(child, b, Some(start));
            }
            // A stray <li> outside any list: treat as a bullet.
            "li" => b.list_item(format!("• {}", collect_text(child)), 12.0),
            "table" => b.table_spanned(parse_table(child), 12.0),
            "img" => emit_image(el, b, base),
            // Containers: recurse to preserve document order.
            _ => walk(child, b, base),
        }
    }
}

/// Flow an `<img>` as an image element: resolve its `src` to bytes (`data:` URI
/// or a `base`-relative file; remote URLs skipped), size from `width`/`height`
/// attrs (CSS px → pt), and carry `alt` as the caption.
fn emit_image(el: &scraper::node::Element, b: &mut PageBuilder, base: Option<&Path>) {
    let Some((bytes, mime)) = image_bytes(el, base) else {
        return;
    };
    let w = px_attr(el, "width").unwrap_or(240.0);
    let h = px_attr(el, "height").unwrap_or(180.0);
    let caption = el
        .attr("alt")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| (s.to_string(), "alt"));
    b.image(bytes, w, h, mime, caption);
}

/// Resolve an `<img src>` to `(bytes, mime)`: decode a `data:` URI, or read a
/// relative path against `base`. Remote `http(s)://` (and protocol-relative
/// `//`) sources are not fetched — they yield `None`.
fn image_bytes(el: &scraper::node::Element, base: Option<&Path>) -> Option<(Vec<u8>, String)> {
    let src = el.attr("src")?.trim();
    if let Some(rest) = src.strip_prefix("data:") {
        // data:[<mime>][;base64],<payload> — only base64 image payloads.
        let (header, payload) = rest.split_once(',')?;
        if !header.contains("base64") {
            return None;
        }
        let mime = header
            .split(';')
            .next()
            .filter(|m| !m.is_empty())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok()?;
        (!bytes.is_empty()).then_some((bytes, mime))
    } else if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//") {
        None // remote: not fetched (offline, deterministic)
    } else {
        // Relative/local path, resolved against the HTML file's directory.
        let base = base?;
        let clean = src.split(['?', '#']).next().unwrap_or(src);
        let bytes = std::fs::read(base.join(clean)).ok()?;
        (!bytes.is_empty()).then_some((bytes, image_mime_from_path(clean)))
    }
}

/// A pixel-valued HTML length attribute (`width`/`height`) → PDF points
/// (CSS px = 1/96 in, pt = 1/72 in, so pt = px × 0.75). Bare number or `…px`.
fn px_attr(el: &scraper::node::Element, name: &str) -> Option<f32> {
    let v = el.attr(name)?.trim();
    let digits: String = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits
        .parse::<f32>()
        .ok()
        .filter(|n| *n > 0.0)
        .map(|px| px * 0.75)
}

/// Emit each direct `<li>` of a list as a tagged list item: bullets get `• `,
/// ordered lists get `N. ` (starting at `ordered_start`). The `LI` tag keeps
/// downstream from reading an ordinal item as a numbered heading. Nested lists
/// fold into their parent item's text (flattened, matching the other backends).
fn walk_list(list: NodeRef<Node>, b: &mut PageBuilder, ordered_start: Option<u64>) {
    let mut n = ordered_start.unwrap_or(0);
    for child in list.children() {
        let Node::Element(el) = child.value() else {
            continue;
        };
        if el.name() != "li" {
            continue;
        }
        let marker = match ordered_start {
            Some(_) => {
                let m = format!("{n}. ");
                n += 1;
                m
            }
            None => "• ".to_string(),
        };
        b.list_item(format!("{marker}{}", collect_text(child)), 12.0);
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

/// Build a sparse span grid from a `<table>`: each descendant `tr` is a row,
/// each `td`/`th` a cell carrying its `colspan`/`rowspan` (covered positions are
/// omitted in HTML — `table_spanned` materializes them). Nested tables flatten
/// into their cell text.
fn parse_table(table: NodeRef<Node>) -> Vec<Vec<SpanCell>> {
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
                    let span = |name| {
                        c.attr(name)
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(1)
                            .max(1)
                    };
                    row.push(SpanCell {
                        text: collect_text(cell),
                        row_span: span("rowspan"),
                        col_span: span("colspan"),
                    });
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
    use docparse_core::ir::{Element, ImageKind};

    /// A 1×1 PNG, base64-encoded — enough to exercise the decode/passthrough.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn first_image(doc: &Document) -> Option<&docparse_core::ir::ImageChunk> {
        doc.pages
            .iter()
            .flat_map(|p| &p.elements)
            .find_map(|e| match e {
                Element::Image(i) => Some(i),
                _ => None,
            })
    }

    #[test]
    fn data_uri_image_becomes_image_element_with_alt_caption() {
        let html = format!(
            "<p>before</p><img src=\"data:image/png;base64,{PNG_B64}\" alt=\"A red square\" width=\"320\" height=\"240\"><p>after</p>"
        );
        let doc = parse_str(&html);
        let img = first_image(&doc).expect("an image element from the data URI");
        assert_eq!(img.kind, ImageKind::Encoded);
        assert_eq!(img.data_media_type.as_deref(), Some("image/png"));
        assert_eq!(img.caption.as_deref(), Some("A red square"));
        assert_eq!(img.caption_source.as_deref(), Some("alt"));
        assert!(!img.data.is_empty());
    }

    #[test]
    fn remote_image_is_not_fetched() {
        let doc = parse_str("<img src=\"https://example.com/x.png\" alt=\"remote\">");
        assert!(
            first_image(&doc).is_none(),
            "remote URLs must not be fetched"
        );
    }

    #[test]
    fn relative_image_resolved_against_base_dir() {
        let dir = std::env::temp_dir().join("docparse_html_relimg_test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(PNG_B64)
            .unwrap();
        std::fs::write(dir.join("pic.png"), &png).unwrap();
        let html_path = dir.join("doc.html");
        std::fs::write(
            &html_path,
            "<img src=\"pic.png\" alt=\"local\" width=\"320\" height=\"240\">",
        )
        .unwrap();
        let doc = HtmlParser.parse(&html_path).unwrap();
        let img = first_image(&doc).expect("an image element from the relative path");
        assert_eq!(img.data, png);
        assert_eq!(img.data_media_type.as_deref(), Some("image/png"));
        assert_eq!(img.caption.as_deref(), Some("local"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_str_without_base_skips_relative_image() {
        // No base dir → a relative <img> can't be read; only data: URIs work.
        let doc = parse_str("<img src=\"pic.png\" alt=\"x\">");
        assert!(first_image(&doc).is_none());
    }

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
    fn collapse_ws_normalizes_runs() {
        assert_eq!(collapse_ws("  a \n\t b   c "), "a b c");
        assert_eq!(collapse_ws(""), "");
    }

    #[test]
    fn heading_levels_map_to_descending_sizes() {
        let doc = parse_str("<h1>a</h1><h2>b</h2><h3>c</h3><h4>d</h4><h5>e</h5><h6>f</h6>");
        let sizes: Vec<f32> = texts(&doc).into_iter().map(|(_, s)| s).collect();
        assert_eq!(sizes.len(), 6);
        // Strictly descending h1..h6, all above the 12.0 body size.
        for w in sizes.windows(2) {
            assert!(w[0] > w[1], "{sizes:?}");
        }
        assert!(*sizes.last().unwrap() >= 12.0);
    }

    fn text_tags(doc: &Document) -> Vec<(String, Option<String>)> {
        doc.pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.tag.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bullet_list_items_get_marker_and_li_tag() {
        let doc = parse_str("<ul><li>one</li><li>two</li></ul>");
        assert_eq!(
            text_tags(&doc),
            vec![
                ("• one".to_string(), Some("LI".to_string())),
                ("• two".to_string(), Some("LI".to_string())),
            ]
        );
    }

    #[test]
    fn ordered_list_numbers_items_and_honors_start() {
        // <ol> items are numbered (not bulleted), and start= is honored.
        let doc = parse_str("<ol start=\"3\"><li>c</li><li>d</li></ol>");
        let t: Vec<(String, Option<String>)> = text_tags(&doc);
        assert_eq!(
            t,
            vec![
                ("3. c".to_string(), Some("LI".to_string())),
                ("4. d".to_string(), Some("LI".to_string())),
            ]
        );
    }

    #[test]
    fn inline_formatting_is_flattened_with_ws_collapsed() {
        let doc = parse_str("<p>Hello <b>brave</b>\n  <i>new</i> world</p>");
        let t = texts(&doc);
        assert_eq!(t[0].0, "Hello brave new world");
    }

    #[test]
    fn nested_table_flattens_into_cell_text() {
        let html = "<table><tr><td>outer <table><tr><td>inner</td></tr></table></td></tr></table>";
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
        // The single top-level table; nested cell text folds into the outer cell.
        assert!(tables[0].rows[0][0].text.contains("outer"));
        assert!(tables[0].rows[0][0].text.contains("inner"));
    }

    #[test]
    fn table_spans_colspan_and_rowspan() {
        // A colspan header and a rowspan body cell: spans/merged land in the IR
        // flat grid, covered positions replicate the anchor text.
        let html = "<table>\
            <tr><th colspan=\"2\">Header</th></tr>\
            <tr><td rowspan=\"2\">A</td><td>b1</td></tr>\
            <tr><td>c2</td></tr>\
            </table>";
        let doc = parse_str(html);
        let table = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .find_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(table.rows.len(), 3);
        assert!(table.rows.iter().all(|r| r.len() == 2), "rectangular 2-col");
        // colspan header anchor + covered position.
        assert_eq!(
            (table.rows[0][0].col_span, table.rows[0][0].merged),
            (2, false)
        );
        assert_eq!(table.rows[0][0].text, "Header");
        assert!(table.rows[0][1].merged);
        // rowspan anchor + covered position below (replicated text).
        assert_eq!(
            (table.rows[1][0].row_span, table.rows[1][0].merged),
            (2, false)
        );
        assert_eq!(table.rows[1][1].text, "b1");
        assert!(table.rows[2][0].merged);
        assert_eq!(table.rows[2][0].text, "A");
        assert_eq!(table.rows[2][1].text, "c2");
    }
}
