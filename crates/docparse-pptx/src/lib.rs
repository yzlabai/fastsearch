//! PPTX backend: one synthetic page per slide. Text frames become paragraphs
//! (title placeholders render heading-sized), `a:tbl` tables map to IR tables.
//! Zip-bomb pre-check shared with the other OOXML backends.

use anyhow::Context;
use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::{emu_to_pt, image_mime_from_path, PageBuilder, SpanCell};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::collections::HashMap;
use std::io::{Read, Seek};
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

    // Pre-load embedded media (rId is resolved per slide via its .rels).
    let media = load_media(&mut zip);

    let mut b = PageBuilder::letter();
    for (_, name) in slides {
        let mut xml = String::new();
        zip.by_name(&name)?.read_to_string(&mut xml)?;
        // Resolve this slide's rId → media path map from its .rels (if any).
        let rid_to_path = match slide_rels_path(&name) {
            Some(rp) => match zip.by_name(&rp) {
                Ok(mut f) => {
                    let mut s = String::new();
                    let _ = f.read_to_string(&mut s);
                    parse_rels(&s, &name)
                }
                Err(_) => HashMap::new(),
            },
            None => HashMap::new(),
        };
        parse_slide(&xml, &rid_to_path, &media, &mut b);
        b.page_break();
    }
    Ok(Document {
        source: "<pptx>".to_string(),
        provenance: Some(Provenance::new("pptx", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    })
}

/// Walk one slide's DrawingML: `a:p` paragraphs (heading-sized for title
/// placeholders), `a:tbl` tables, `p:pic` pictures (resolved to media bytes via
/// `rid_to_path` + `media`).
fn parse_slide(
    xml: &str,
    rid_to_path: &HashMap<String, String>,
    media: &HashMap<String, Vec<u8>>,
    b: &mut PageBuilder,
) {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);

    let mut para = String::new();
    let mut in_title = false;
    // table state
    let mut table: Option<Vec<Vec<SpanCell>>> = None;
    let mut row: Vec<SpanCell> = Vec::new();
    let mut cell = String::new();
    // span state of the current <a:tc> (DrawingML declares both spans on the
    // anchor; covered positions are explicit hMerge/vMerge cells we drop).
    let mut col_span = 1u32;
    let mut row_span = 1u32;
    let mut covered = false;
    // picture state of the current <p:pic>: its xfrm extent (EMU) + blip rId.
    let mut in_pic = false;
    let mut pic_cx = 0u32;
    let mut pic_cy = 0u32;
    let mut pic_rid: Option<String> = None;
    let mut got_ext = false;

    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"pic" => {
                    in_pic = true;
                    pic_cx = 0;
                    pic_cy = 0;
                    pic_rid = None;
                    got_ext = false;
                }
                // The picture's xfrm extent (first <a:ext> inside the <p:pic>).
                b"ext" if in_pic && !got_ext => {
                    pic_cx = attr_u32(&e, b"cx").unwrap_or(0);
                    pic_cy = attr_u32(&e, b"cy").unwrap_or(0);
                    got_ext = true;
                }
                // <a:blip r:embed="rIdN"/> names the media relationship.
                b"blip" if in_pic => {
                    if let Some(id) = attr_str(&e, b"embed") {
                        pic_rid = Some(id);
                    }
                }
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
                b"tc" if table.is_some() => {
                    cell.clear();
                    col_span = attr_u32(&e, b"gridSpan").unwrap_or(1).max(1);
                    row_span = attr_u32(&e, b"rowSpan").unwrap_or(1).max(1);
                    // hMerge/vMerge mark a position covered by an anchor to the
                    // left/above; expand_spans regenerates it from the anchor.
                    covered = attr_flag(&e, b"hMerge") || attr_flag(&e, b"vMerge");
                }
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
                b"pic" => {
                    in_pic = false;
                    // Resolve the blip's rId to media bytes and place the image
                    // at this flow position. Missing extent → a sane default box.
                    if let Some(bytes) = pic_rid
                        .take()
                        .and_then(|rid| rid_to_path.get(&rid))
                        .and_then(|path| media.get(path).map(|b| (b, path)))
                        .filter(|(b, _)| !b.is_empty())
                    {
                        let (data, path) = bytes;
                        let w = if pic_cx > 0 { emu_to_pt(pic_cx) } else { 216.0 };
                        let h = if pic_cy > 0 { emu_to_pt(pic_cy) } else { 144.0 };
                        b.image(data.clone(), w, h, image_mime_from_path(path));
                    }
                }
                b"p" => {
                    if table.is_none() && !para.trim().is_empty() {
                        b.paragraph(para.trim(), if in_title { 18.0 } else { 11.0 });
                    }
                    para.clear();
                }
                b"sp" => in_title = false, // shape ends
                b"tc" => {
                    let text = std::mem::take(&mut cell).trim().to_string();
                    // Covered positions are dropped; the anchor's spans let
                    // expand_spans rematerialize them with the replicated text.
                    if !covered {
                        row.push(SpanCell {
                            text,
                            row_span,
                            col_span,
                        });
                    }
                }
                b"tr" => {
                    if let Some(t) = table.as_mut() {
                        t.push(std::mem::take(&mut row));
                    }
                }
                b"tbl" => {
                    if let Some(rows) = table.take() {
                        if !rows.is_empty() {
                            b.table_spanned(rows, 10.0);
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

/// Parse an unprefixed attribute of `e` as a `u32` (e.g. `gridSpan="2"`).
fn attr_u32(e: &BytesStart, name: &[u8]) -> Option<u32> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.unescape_value().ok())
        .and_then(|v| v.parse().ok())
}

/// True if attribute `name` is present and truthy (`"1"`/`"true"`) — the
/// DrawingML `hMerge`/`vMerge` covered-cell flags.
fn attr_flag(e: &BytesStart, name: &[u8]) -> bool {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.unescape_value().ok())
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Read an unprefixed attribute of `e` as an owned string (e.g. relationship
/// `Id`/`Target`, blip `embed`).
fn attr_str(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.unescape_value().ok())
        .map(|v| v.into_owned())
}

/// Load every `ppt/media/*` archive entry into a path → bytes map.
fn load_media<R: Read + Seek>(zip: &mut zip::ZipArchive<R>) -> HashMap<String, Vec<u8>> {
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            let n = zip.by_index(i).ok()?.name().to_string();
            n.starts_with("ppt/media/").then_some(n)
        })
        .collect();
    let mut map = HashMap::new();
    for n in names {
        if let Ok(mut f) = zip.by_name(&n) {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                map.insert(n, buf);
            }
        }
    }
    map
}

/// The rels path for a slide: `ppt/slides/slide1.xml` → `ppt/slides/_rels/slide1.xml.rels`.
fn slide_rels_path(slide_name: &str) -> Option<String> {
    let (dir, file) = slide_name.rsplit_once('/')?;
    Some(format!("{dir}/_rels/{file}.rels"))
}

/// Parse a `.rels` document into rId → resolved-media-path. Targets are resolved
/// relative to the slide's directory (e.g. `../media/image1.jpeg` from
/// `ppt/slides/…` → `ppt/media/image1.jpeg`).
fn parse_rels(xml: &str, slide_name: &str) -> HashMap<String, String> {
    let base_dir = slide_name.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let mut map = HashMap::new();
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                if let (Some(id), Some(target)) = (attr_str(&e, b"Id"), attr_str(&e, b"Target")) {
                    if let Some(path) = resolve_target(base_dir, &target) {
                        map.insert(id, path);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

/// Resolve a relationship `Target` against the referencing part's directory,
/// collapsing `.`/`..`. An absolute target (`/ppt/…`) is taken as package-root.
fn resolve_target(base_dir: &str, target: &str) -> Option<String> {
    if let Some(abs) = target.strip_prefix('/') {
        return Some(abs.to_string());
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::{parse_bytes, resolve_target};
    use docparse_core::ir::{Element, ImageKind};
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

    /// Build a PPTX zip from arbitrary (path, bytes) entries.
    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts: zip::write::SimpleFileOptions = Default::default();
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn rels_target_resolves_relative_to_slide_dir() {
        assert_eq!(
            resolve_target("ppt/slides", "../media/image1.png").as_deref(),
            Some("ppt/media/image1.png")
        );
        assert_eq!(
            resolve_target("ppt/slides", "/ppt/media/x.jpeg").as_deref(),
            Some("ppt/media/x.jpeg")
        );
    }

    #[test]
    fn slide_picture_becomes_image_element() {
        let png = vec![137u8, 80, 78, 71, 13, 10, 26, 10, 1, 2, 3]; // PNG sig + payload
        let slide = r#"<p:sld xmlns:a="a" xmlns:p="p" xmlns:r="r"><p:pic>
            <p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="457200"/></a:xfrm></p:spPr>
            <p:blipFill><a:blip r:embed="rId2"/></p:blipFill></p:pic></p:sld>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/></Relationships>"#;
        let buf = zip_bytes(&[
            ("ppt/slides/slide1.xml", slide.as_bytes()),
            ("ppt/slides/_rels/slide1.xml.rels", rels.as_bytes()),
            ("ppt/media/image1.png", &png),
        ]);
        let doc = parse_bytes(&buf).unwrap();
        let img = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .find_map(|e| match e {
                Element::Image(i) => Some(i),
                _ => None,
            })
            .expect("an image element from the slide picture");
        assert_eq!(img.kind, ImageKind::Encoded);
        assert_eq!(img.data, png);
        assert_eq!(img.data_media_type.as_deref(), Some("image/png"));
        // 1in × 0.5in (EMU) → 72pt × 36pt.
        assert!(
            (img.bbox.width() - 72.0).abs() < 1.0,
            "w={}",
            img.bbox.width()
        );
        assert!(
            (img.bbox.height() - 36.0).abs() < 1.0,
            "h={}",
            img.bbox.height()
        );
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

    fn slide_texts(doc: &docparse_core::ir::Document, page: usize) -> Vec<(String, f32)> {
        doc.pages[page]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.font_size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ctr_title_placeholder_is_also_heading_sized() {
        let s = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:sp><p:ph type="ctrTitle"/><a:p><a:r><a:t>Cover</a:t></a:r></a:p></p:sp></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let t = slide_texts(&doc, 0);
        assert!(t.iter().any(|(s, sz)| s == "Cover" && *sz > 15.0), "{t:?}");
    }

    #[test]
    fn multiple_runs_in_a_cell_join_with_space() {
        let s = r#"<p:sld xmlns:a="a"><a:tbl><a:tr><a:tc><a:p><a:r><a:t>foo</a:t></a:r><a:r><a:t>bar</a:t></a:r></a:p></a:tc></a:tr></a:tbl></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let tables: Vec<_> = doc.pages[0]
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables[0].rows[0][0].text, "foo bar");
    }

    #[test]
    fn empty_paragraphs_are_dropped() {
        let s = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:sp><a:p></a:p></p:sp><p:sp><a:p><a:r><a:t>real</a:t></a:r></a:p></p:sp></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let t = slide_texts(&doc, 0);
        assert_eq!(t.iter().filter(|(s, _)| s == "real").count(), 1);
        assert!(t.iter().all(|(s, _)| !s.trim().is_empty()), "{t:?}");
    }

    #[test]
    fn malformed_slide_xml_does_not_panic() {
        // Truncated mid-tag: parser must keep what it had and never panic.
        let s = r#"<p:sld xmlns:a="a"><p:sp><a:p><a:r><a:t>partial</a:t></a:r></a:p></p:sp><a:tbl><a:tr><a:tc"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let t = slide_texts(&doc, 0);
        assert!(t.iter().any(|(s, _)| s == "partial"), "{t:?}");
    }

    fn first_table(doc: &docparse_core::ir::Document) -> &docparse_core::ir::Table {
        doc.pages[0]
            .elements
            .iter()
            .find_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table")
    }

    // TC-P5a: gridSpan on the anchor + an hMerge covered cell → col_span + a
    // replicated covered position.
    #[test]
    fn table_gridspan_becomes_colspan() {
        let s = r#"<p:sld xmlns:a="a"><a:tbl>
            <a:tr><a:tc gridSpan="2"><a:t>Wide</a:t></a:tc><a:tc hMerge="1"><a:t></a:t></a:tc><a:tc><a:t>C</a:t></a:tc></a:tr>
            <a:tr><a:tc><a:t>a</a:t></a:tc><a:tc><a:t>b</a:t></a:tc><a:tc><a:t>c</a:t></a:tc></a:tr>
            </a:tbl></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let table = first_table(&doc);
        assert!(table.rows.iter().all(|r| r.len() == 3), "3-col grid");
        assert_eq!(
            (table.rows[0][0].col_span, table.rows[0][0].merged),
            (2, false)
        );
        assert_eq!(table.rows[0][0].text, "Wide");
        assert!(table.rows[0][1].merged);
        assert_eq!(table.rows[0][1].text, "Wide", "covered text replicated");
        assert_eq!(table.rows[0][2].text, "C");
        assert_eq!(table.rows[1][0].text, "a");
    }

    // TC-P5b: rowSpan on the anchor + a vMerge covered cell below → row_span + a
    // replicated covered position.
    #[test]
    fn table_rowspan_via_vmerge_becomes_rowspan() {
        let s = r#"<p:sld xmlns:a="a"><a:tbl>
            <a:tr><a:tc rowSpan="2"><a:t>Tall</a:t></a:tc><a:tc><a:t>b1</a:t></a:tc></a:tr>
            <a:tr><a:tc vMerge="1"><a:t></a:t></a:tc><a:tc><a:t>c2</a:t></a:tc></a:tr>
            </a:tbl></p:sld>"#;
        let doc = parse_bytes(&pptx_with(&[s])).unwrap();
        let table = first_table(&doc);
        assert_eq!(
            (table.rows[0][0].row_span, table.rows[0][0].merged),
            (2, false)
        );
        assert_eq!(table.rows[0][0].text, "Tall");
        assert_eq!(table.rows[0][1].text, "b1");
        assert!(table.rows[1][0].merged);
        assert_eq!(
            table.rows[1][0].text, "Tall",
            "covered text replicated down"
        );
        assert_eq!(table.rows[1][1].text, "c2");
    }
}
