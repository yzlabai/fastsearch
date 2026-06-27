//! Tagged-PDF structure tree (G9a): author-declared document semantics.
//!
//! Accessible PDFs carry a `StructTreeRoot` whose depth-first traversal is the
//! author's reading order, and whose `StructElem` roles (`H1`..`H6`, `P`, `L`,
//! `LI`, `Table`, …) are ground-truth semantics. Content links to the tree via
//! marked-content IDs (`BDC /P <</MCID n>>` in the content stream). This
//! module walks the tree once per document and produces, per page, a map
//! `MCID -> (normalized role, traversal order)`. The interpreter applies the
//! ROLE to text chunks (`TextChunk.tag`); the traversal ORDER is computed but
//! deliberately NOT applied as reading order — real-world tag trees are
//! authored in creation order, not visual order (measured −0.15 NID on amt;
//! see the G9a devlog).
//!
//! Untagged documents produce an empty map — zero behavior change.

use lopdf::{Dictionary, Document as PdfDocument, Object, ObjectId};
use std::collections::HashMap;

/// Per-page tag map: MCID → (role, traversal order).
pub type PageTags = HashMap<u32, (String, u32)>;

/// Maximum tree depth walked (cycle/abuse guard).
const MAX_DEPTH: usize = 64;

/// Parse the structure tree. Returns per-page-number tag maps; empty when the
/// document is untagged or the tree is malformed (never an error — tags are
/// an enhancement over the geometric pipeline, not a requirement).
pub fn build_page_tags(doc: &PdfDocument) -> HashMap<usize, PageTags> {
    let mut out: HashMap<usize, PageTags> = HashMap::new();
    let Some(root) = struct_tree_root(doc) else {
        return out;
    };
    // Page object id -> page number.
    let page_no: HashMap<ObjectId, usize> = doc
        .get_pages()
        .into_iter()
        .map(|(n, id)| (id, n as usize))
        .collect();
    let role_map = role_map(doc, &root);

    let mut order = 0u32;
    if let Ok(kids) = root.get(b"K") {
        walk(
            doc, kids, None, None, &page_no, &role_map, &mut out, &mut order, 0,
        );
    }
    out
}

fn struct_tree_root(doc: &PdfDocument) -> Option<Dictionary> {
    let catalog = doc.catalog().ok()?;
    deref_dict(doc, catalog.get(b"StructTreeRoot").ok()?)
}

/// `/RoleMap` translates custom role names to standard ones.
fn role_map(doc: &PdfDocument, root: &Dictionary) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(rm) = root.get(b"RoleMap").ok().and_then(|o| deref_dict(doc, o)) {
        for (k, v) in rm.iter() {
            if let Ok(name) = v.as_name() {
                map.insert(
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(name).into_owned(),
                );
            }
        }
    }
    map
}

/// Depth-first walk. `role` is the nearest enclosing StructElem role; `page`
/// the nearest enclosing /Pg.
#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &PdfDocument,
    node: &Object,
    role: Option<&str>,
    page: Option<ObjectId>,
    page_no: &HashMap<ObjectId, usize>,
    roles: &HashMap<String, String>,
    out: &mut HashMap<usize, PageTags>,
    order: &mut u32,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match node {
        // A bare integer kid is an MCID on the enclosing page.
        Object::Integer(mcid) => {
            record(out, page, page_no, *mcid, role, order);
        }
        Object::Array(items) => {
            for it in items {
                walk(doc, it, role, page, page_no, roles, out, order, depth + 1);
            }
        }
        Object::Reference(_) => {
            if let Ok((_, resolved)) = doc.dereference(node) {
                // Guard against self-referencing objects via depth cap.
                walk(
                    doc,
                    resolved,
                    role,
                    page,
                    page_no,
                    roles,
                    out,
                    order,
                    depth + 1,
                );
            }
        }
        Object::Dictionary(d) => {
            let dtype = d
                .get(b"Type")
                .and_then(|o| o.as_name())
                .unwrap_or(b"StructElem");
            if dtype == b"MCR" {
                // Marked-content reference: explicit /MCID (+ optional /Pg).
                let pg = d
                    .get(b"Pg")
                    .ok()
                    .and_then(|o| o.as_reference().ok())
                    .or(page);
                if let Ok(mcid) = d.get(b"MCID").and_then(|o| o.as_i64()) {
                    record(out, pg, page_no, mcid, role, order);
                }
                return;
            }
            if dtype == b"OBJR" {
                return; // object reference (annotation etc.) — no text MCIDs
            }
            // StructElem: adopt its role (RoleMap-normalized) and /Pg.
            let own_role = d
                .get(b"S")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .map(|r| roles.get(&r).cloned().unwrap_or(r));
            let own_page = d
                .get(b"Pg")
                .ok()
                .and_then(|o| o.as_reference().ok())
                .or(page);
            if let Ok(kids) = d.get(b"K") {
                walk(
                    doc,
                    kids,
                    own_role.as_deref().or(role),
                    own_page,
                    page_no,
                    roles,
                    out,
                    order,
                    depth + 1,
                );
            }
        }
        _ => {}
    }
}

fn record(
    out: &mut HashMap<usize, PageTags>,
    page: Option<ObjectId>,
    page_no: &HashMap<ObjectId, usize>,
    mcid: i64,
    role: Option<&str>,
    order: &mut u32,
) {
    let (Some(pid), Ok(mcid)) = (page, u32::try_from(mcid)) else {
        return;
    };
    let Some(&no) = page_no.get(&pid) else {
        return;
    };
    let entry = out.entry(no).or_default();
    entry.entry(mcid).or_insert_with(|| {
        let o = *order;
        *order += 1;
        (role.unwrap_or("P").to_string(), o)
    });
}

fn deref_dict(doc: &PdfDocument, o: &Object) -> Option<Dictionary> {
    match doc.dereference(o) {
        Ok((_, Object::Dictionary(d))) => Some(d.clone()),
        _ => None,
    }
}
