//! PPTX backend: one synthetic page per slide. Text frames become paragraphs
//! (title placeholders render heading-sized), `a:tbl` tables map to IR tables.
//! Zip-bomb pre-check shared with the other OOXML backends.

use anyhow::Context;
use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::Read;
use std::path::Path;

pub struct PptxParser;

impl DocumentParser for PptxParser {
    fn name(&self) -> &'static str {
        "pptx"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pptx"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let buf = std::fs::read(path)?;
        let mut doc = parse_bytes(&buf)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse PPTX bytes into a [`Document`].
pub fn parse_bytes(buf: &[u8]) -> anyhow::Result<Document> {
    docparse_core::limits::check_zip_bomb(buf)?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(buf)).context("pptx zip")?;

    // Slides in numeric order (slide1.xml, slide2.xml, …).
    let mut slides: Vec<(u32, String)> = (0..zip.len())
        .filter_map(|i| {
            let name = zip.by_index(i).ok()?.name().to_string();
            let n = name
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((n, name))
        })
        .collect();
    slides.sort();

    let mut b = PageBuilder::letter();
    for (_, name) in slides {
        let mut xml = String::new();
        zip.by_name(&name)?.read_to_string(&mut xml)?;
        parse_slide(&xml, &mut b);
        b.page_break();
    }
    Ok(Document {
        source: "<pptx>".to_string(),
        provenance: Some(Provenance::new("pptx", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    })
}

/// Walk one slide's DrawingML: `a:p` paragraphs (heading-sized for title
/// placeholders), `a:tbl` tables.
fn parse_slide(xml: &str, b: &mut PageBuilder) {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);

    let mut para = String::new();
    let mut in_title = false;
    // table state
    let mut table: Option<Vec<Vec<String>>> = None;
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"ph" => {
                    // <p:ph type="title|ctrTitle"> marks the shape as a title.
                    if let Some(t) = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == b"type")
                    {
                        let v = t.unescape_value().unwrap_or_default();
                        if v == "title" || v == "ctrTitle" {
                            in_title = true;
                        }
                    }
                }
                b"tbl" => table = Some(Vec::new()),
                b"tr" if table.is_some() => row.clear(),
                b"tc" if table.is_some() => cell.clear(),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let txt = t.unescape().unwrap_or_default();
                if table.is_some() {
                    if !cell.is_empty() {
                        cell.push(' ');
                    }
                    cell.push_str(&txt);
                } else {
                    if !para.is_empty() {
                        para.push(' ');
                    }
                    para.push_str(&txt);
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"p" => {
                    if table.is_none() && !para.trim().is_empty() {
                        b.paragraph(para.trim(), if in_title { 18.0 } else { 11.0 });
                    }
                    para.clear();
                }
                b"sp" => in_title = false, // shape ends
                b"tc" => row.push(std::mem::take(&mut cell).trim().to_string()),
                b"tr" => {
                    if let Some(t) = table.as_mut() {
                        t.push(std::mem::take(&mut row));
                    }
                }
                b"tbl" => {
                    if let Some(rows) = table.take() {
                        if !rows.is_empty() {
                            b.table(rows, 10.0);
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break, // malformed slide: keep what we have, never panic
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_bytes;
    use docparse_core::ir::Element;
    use std::io::Write;

    fn pptx_with(slides: &[&str]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (i, s) in slides.iter().enumerate() {
            zw.start_file(format!("ppt/slides/slide{}.xml", i + 1), opts)
                .unwrap();
            zw.write_all(s.as_bytes()).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn slides_titles_and_tables() {
        let s1 = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:sp><p:ph type="title"/><a:p><a:r><a:t>Deck Title</a:t></a:r></a:p></p:sp>
            <p:sp><a:p><a:r><a:t>Bullet one</a:t></a:r></a:p></p:sp></p:sld>"#;
        let s2 = r#"<p:sld xmlns:a="a"><a:tbl><a:tr><a:tc><a:t>H1</a:t></a:tc><a:tc><a:t>H2</a:t></a:tc></a:tr>
            <a:tr><a:tc><a:t>1</a:t></a:tc><a:tc><a:t>2</a:t></a:tc></a:tr></a:tbl></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s1, s2])).unwrap();
        assert_eq!(doc.pages.len(), 2, "one page per slide");
        let texts: Vec<(String, f32)> = doc.pages[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.font_size)),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|(t, s)| t == "Deck Title" && *s > 15.0),
            "{texts:?}"
        );
        assert!(texts.iter().any(|(t, _)| t == "Bullet one"));
        let tables: Vec<_> = doc.pages[1]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 2);
        assert_eq!(tables[0].rows[0][0].text, "H1");
    }
}
