//! PDF document outline (`/Outlines`, a.k.a. bookmarks) → heading structure.
//!
//! Books, reports and many papers ship an author-authored table of contents in
//! the catalog's `/Outlines` tree: nested entries, each a `/Title` plus a
//! `/Dest` (or `/A` GoTo action) pointing at a page. That nesting IS the logical
//! hierarchy — more reliable than font-size heuristics. Rather than carry it as
//! a new IR field (which every parser's `Document` literal would have to grow),
//! we fold it into the *existing* tagged-heading channel: anchor each bookmark
//! to the matching heading text on its destination page and stamp that text with
//! a `tag` of `"H<level>"`. Downstream, [`docparse_core::layout`] already turns
//! an `H1`..`H6` tag into a heading at that level (overriding font geometry), so
//! the structure tree ([`docparse_core::outline`]) and chunk breadcrumbs pick up
//! the author's hierarchy for free — and documents without bookmarks are
//! untouched (byte-identical output).
//!
//! Matching is deliberately high-precision: a bookmark only tags text when a
//! contiguous run of chunks on its page joins to *exactly* the (whitespace-
//! normalized) title. A miss degrades silently to the geometric heading path —
//! we never invent a heading we can't see, and never override an existing tag.

use docparse_core::ir::{Element, Page};
use lopdf::{Dictionary, Document as PdfDocument, Object, ObjectId};
use std::collections::HashMap;

/// One outline entry: its title, 1-based destination page, and nesting level
/// (1 = top-level bookmark). `page` is 0 when the destination can't be resolved.
pub struct Bookmark {
    pub title: String,
    pub page: usize,
    pub level: u8,
}

/// Walk the catalog `/Outlines` tree into a flat, reading-order bookmark list.
/// Empty when the document has no outline (the common case for born-digital
/// papers without a TOC) or the tree is malformed — never an error.
pub fn build_bookmarks(doc: &PdfDocument) -> Vec<Bookmark> {
    let Ok(catalog) = doc.catalog() else {
        return Vec::new();
    };
    let Some(outlines) = catalog
        .get(b"Outlines")
        .ok()
        .and_then(|o| deref_dict(doc, o))
    else {
        return Vec::new();
    };
    let page_no: HashMap<ObjectId, usize> = doc
        .get_pages()
        .into_iter()
        .map(|(n, id)| (id, n as usize))
        .collect();
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    if let Ok(first) = outlines.get(b"First") {
        walk(doc, first, 1, &page_no, &mut out, &mut visited);
    }
    out
}

/// Maximum bookmarks collected (abuse guard) and maximum nesting depth.
const MAX_BOOKMARKS: usize = 10_000;
const MAX_DEPTH: u8 = 32;

fn walk(
    doc: &PdfDocument,
    node: &Object,
    level: u8,
    page_no: &HashMap<ObjectId, usize>,
    out: &mut Vec<Bookmark>,
    visited: &mut std::collections::HashSet<ObjectId>,
) {
    if level > MAX_DEPTH || out.len() >= MAX_BOOKMARKS {
        return;
    }
    // Follow the /Next sibling chain; recurse into /First children.
    let mut cur = node.clone();
    loop {
        // Track the object id to break cycles in malformed trees.
        if let Ok(id) = as_ref(&cur) {
            if !visited.insert(id) {
                return;
            }
        }
        let Some(item) = deref_dict(doc, &cur) else {
            return;
        };
        if let Some(title) = item
            .get(b"Title")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_str().ok().map(decode_pdf_string))
        {
            let title = title.trim().to_string();
            if !title.is_empty() {
                let page = resolve_dest_page(doc, &item, page_no).unwrap_or(0);
                out.push(Bookmark { title, page, level });
            }
        }
        if let Ok(first) = item.get(b"First") {
            walk(doc, first, level + 1, page_no, out, visited);
        }
        match item.get(b"Next") {
            Ok(next) => cur = next.clone(),
            Err(_) => return,
        }
        if out.len() >= MAX_BOOKMARKS {
            return;
        }
    }
}

/// Resolve a bookmark's destination to a 1-based page number. Handles the
/// direct `/Dest` array, a `/A` GoTo action's `/D`, and named destinations
/// (both the legacy catalog `/Dests` dict and the `/Names` → `/Dests` tree).
fn resolve_dest_page(
    doc: &PdfDocument,
    item: &Dictionary,
    page_no: &HashMap<ObjectId, usize>,
) -> Option<usize> {
    let dest = item.get(b"Dest").ok().cloned().or_else(|| {
        // /A << /S /GoTo /D dest >>
        item.get(b"A")
            .ok()
            .and_then(|o| deref_dict(doc, o))
            .and_then(|a| a.get(b"D").ok().cloned())
    })?;
    let dest = doc.dereference(&dest).map(|(_, o)| o.clone()).ok()?;
    let page_obj = match dest {
        // [page /XYZ ...] — first element is the page reference.
        Object::Array(ref a) => a.first().cloned()?,
        // A named destination (string or name) → look it up.
        Object::String(ref s, _) => named_dest(doc, s)?,
        Object::Name(ref n) => named_dest(doc, n)?,
        _ => return None,
    };
    let id = as_ref(&page_obj).ok()?;
    page_no.get(&id).copied()
}

/// Resolve a named destination to its page reference object.
fn named_dest(doc: &PdfDocument, name: &[u8]) -> Option<Object> {
    let catalog = doc.catalog().ok()?;
    // Legacy: catalog /Dests is a dict name -> dest (array or << /D array >>).
    if let Some(dests) = catalog.get(b"Dests").ok().and_then(|o| deref_dict(doc, o)) {
        if let Some(d) = dests.get(name).ok().and_then(|o| dest_array_first(doc, o)) {
            return Some(d);
        }
    }
    // PDF 1.2+: catalog /Names /Dests is a name tree.
    let names = catalog
        .get(b"Names")
        .ok()
        .and_then(|o| deref_dict(doc, o))?;
    let root = names.get(b"Dests").ok()?;
    name_tree_lookup(doc, root, name, 0)
}

/// First element (page ref) of a destination value that is either an array or a
/// `<< /D [page ...] >>` dictionary.
fn dest_array_first(doc: &PdfDocument, o: &Object) -> Option<Object> {
    let (_, o) = doc.dereference(o).ok()?;
    match o {
        Object::Array(a) => a.first().cloned(),
        Object::Dictionary(d) => {
            let d = d.get(b"D").ok()?;
            let (_, d) = doc.dereference(d).ok()?;
            d.as_array().ok()?.first().cloned()
        }
        _ => None,
    }
}

/// Look up `key` in a `/Names` name-tree node (recursing through `/Kids`).
fn name_tree_lookup(doc: &PdfDocument, node: &Object, key: &[u8], depth: u8) -> Option<Object> {
    if depth > MAX_DEPTH {
        return None;
    }
    let dict = deref_dict(doc, node)?;
    // Leaf: /Names [ key1 val1 key2 val2 ... ].
    if let Some(arr) = dict
        .get(b"Names")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
    {
        if let Ok(pairs) = arr.1.as_array() {
            let mut i = 0;
            while i + 1 < pairs.len() {
                if pairs[i].as_str().ok() == Some(key) {
                    return dest_array_first(doc, &pairs[i + 1]);
                }
                i += 2;
            }
        }
    }
    // Intermediate: /Kids [ node ... ].
    if let Some(kids) = dict.get(b"Kids").ok().and_then(|o| doc.dereference(o).ok()) {
        if let Ok(kids) = kids.1.as_array() {
            for kid in kids {
                if let Some(found) = name_tree_lookup(doc, kid, key, depth + 1) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Anchor each bookmark to the matching heading text on its destination page and
/// stamp it with an `H<level>` tag, so the existing tagged-heading pipeline lifts
/// it into the structure tree. No-op for pages/titles that don't match; never
/// overrides a tag the structure tree already set.
pub fn apply_bookmarks(pages: &mut [Page], bookmarks: &[Bookmark]) {
    for bm in bookmarks {
        if bm.page == 0 || bm.title.is_empty() {
            continue;
        }
        let Some(page) = pages.iter_mut().find(|p| p.number == bm.page) else {
            continue;
        };
        tag_heading(page, &bm.title, bm.level);
    }
}

/// Comparison key for matching a bookmark title against on-page heading text:
/// whitespace-collapsed, case-folded, and with a leading section number
/// stripped. Bookmarks routinely omit (or differ in) the section number that
/// the printed heading carries — "Introduction" vs "1 Introduction", "2.1
/// Setup" vs "2.1. Setup" — so normalizing both sides this way anchors them
/// without resorting to loose substring matching (which would false-positive).
fn key(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = collapsed.to_lowercase();
    strip_leading_number(&lower).to_string()
}

/// Drop a leading section number like `1`, `2.1`, `3.2.4`, optionally with a
/// trailing dot/paren, plus the space after it. Leaves the rest untouched.
fn strip_leading_number(s: &str) -> &str {
    let rest = s.trim_start();
    let num_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    // Require at least one digit and a following space to treat it as numbering.
    if num_end > 0 && rest[..num_end].contains(|c: char| c.is_ascii_digit()) {
        let after = rest[num_end..].trim_start_matches([')', '.']);
        if let Some(stripped) = after.strip_prefix(' ') {
            return stripped.trim_start();
        }
        // "1 " with the number directly followed by a space (no punctuation).
        if let Some(stripped) = rest[num_end..].strip_prefix(' ') {
            return stripped.trim_start();
        }
    }
    rest
}

/// Find a contiguous run of (currently untagged) text chunks on `page` whose
/// joined text matches `title` under [`key`] normalization, and tag them all
/// `H<level>`.
fn tag_heading(page: &mut Page, title: &str, level: u8) {
    let target = key(title);
    if target.is_empty() {
        return;
    }
    let tag = format!("H{}", level.clamp(1, 6));
    // Indices of text elements in document order, with their text.
    let texts: Vec<(usize, String)> = page
        .elements
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            Element::Text(t) if t.tag.is_none() && !t.hidden => Some((i, t.text.clone())),
            _ => None,
        })
        .collect();
    // Greedily grow a contiguous run from each start until its key meets or
    // exceeds the target length, accepting only an exact key match.
    for start in 0..texts.len() {
        let mut joined = String::new();
        for (k, (_, t)) in texts[start..].iter().enumerate() {
            if k > 0 {
                joined.push(' ');
            }
            joined.push_str(t);
            let cand = key(&joined);
            if cand.chars().count() > target.chars().count() {
                break; // overshot — this start can't match
            }
            if cand == target {
                for (idx, _) in &texts[start..=start + k] {
                    if let Element::Text(t) = &mut page.elements[*idx] {
                        t.tag = Some(tag.clone());
                    }
                }
                return;
            }
        }
    }
}

/// Dereference an object to a dictionary (following one indirect reference).
fn deref_dict(doc: &PdfDocument, o: &Object) -> Option<Dictionary> {
    doc.dereference(o).ok().and_then(|(_, o)| match o {
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    })
}

/// The object id a `Reference` points to (for cycle detection / page lookup).
fn as_ref(o: &Object) -> Result<ObjectId, ()> {
    match o {
        Object::Reference(id) => Ok(*id),
        _ => Err(()),
    }
}

/// Decode a PDF string: UTF-16BE when it carries a BOM, else PDFDocEncoding
/// approximated by Latin-1 (covers ASCII titles exactly; the common case).
fn decode_pdf_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::{BBox, TextChunk};

    fn text(t: &str, tag: Option<&str>) -> Element {
        Element::Text(TextChunk {
            text: t.into(),
            bbox: BBox {
                x0: 72.0,
                y0: 700.0,
                x1: 200.0,
                y1: 714.0,
            },
            font_size: 12.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: tag.map(String::from),
        })
    }

    fn page(els: Vec<Element>) -> Page {
        Page {
            number: 1,
            width: 612.0,
            height: 792.0,
            elements: els,
        }
    }

    fn tag_of(p: &Page, i: usize) -> Option<String> {
        match &p.elements[i] {
            Element::Text(t) => t.tag.clone(),
            _ => None,
        }
    }

    #[test]
    fn tags_a_single_chunk_heading() {
        let mut p = page(vec![text("3 Methods", None), text("body text", None)]);
        let bm = Bookmark {
            title: "3 Methods".into(),
            page: 1,
            level: 2,
        };
        apply_bookmarks(std::slice::from_mut(&mut p), &[bm]);
        assert_eq!(tag_of(&p, 0).as_deref(), Some("H2"));
        assert_eq!(tag_of(&p, 1), None); // body untouched
    }

    #[test]
    fn tags_a_heading_split_across_chunks_and_normalizes_whitespace() {
        let mut p = page(vec![text("3.2", None), text("Training   Details", None)]);
        let bm = Bookmark {
            title: "3.2 Training Details".into(),
            page: 1,
            level: 3,
        };
        apply_bookmarks(std::slice::from_mut(&mut p), &[bm]);
        assert_eq!(tag_of(&p, 0).as_deref(), Some("H3"));
        assert_eq!(tag_of(&p, 1).as_deref(), Some("H3"));
    }

    #[test]
    fn matches_despite_leading_section_number_mismatch() {
        // Bookmark "Introduction" vs printed heading "1 Introduction" (the
        // common arXiv case): leading-number stripping anchors them.
        let mut p = page(vec![text("1 Introduction", None)]);
        let bm = Bookmark {
            title: "Introduction".into(),
            page: 1,
            level: 1,
        };
        apply_bookmarks(std::slice::from_mut(&mut p), &[bm]);
        assert_eq!(tag_of(&p, 0).as_deref(), Some("H1"));
    }

    #[test]
    fn no_match_leaves_everything_untouched() {
        let mut p = page(vec![text("Something else", None)]);
        let bm = Bookmark {
            title: "Nonexistent Section".into(),
            page: 1,
            level: 1,
        };
        apply_bookmarks(std::slice::from_mut(&mut p), &[bm]);
        assert_eq!(tag_of(&p, 0), None);
    }

    #[test]
    fn never_overrides_an_existing_tag() {
        let mut p = page(vec![text("Intro", Some("P"))]);
        let bm = Bookmark {
            title: "Intro".into(),
            page: 1,
            level: 1,
        };
        apply_bookmarks(std::slice::from_mut(&mut p), &[bm]);
        assert_eq!(tag_of(&p, 0).as_deref(), Some("P")); // structure-tree tag wins
    }

    #[test]
    fn utf16be_title_decodes() {
        // "Hi" in UTF-16BE with BOM.
        let s = decode_pdf_string(&[0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69]);
        assert_eq!(s, "Hi");
    }
}
