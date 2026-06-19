//! Open Knowledge Format (OKF v0.1) bundle export (roadmap module 4 product
//! out / structure-tree iteration).
//!
//! Serializes the document **structure tree** ([`crate::outline`]) joined with
//! the **chunks** ([`crate::chunk`], each tagged with its `section_id`) into a
//! directory of Markdown-with-YAML-frontmatter "concept" files — the厂商-neutral,
//! git-native, citable delivery format defined by Google Cloud's OKF v0.1. The
//! tree gives the skeleton (one concept per section, nested by `children`); the
//! chunks give the flesh (each section's *direct* content). The join happens
//! here at emit time, so `-f json` and the other outputs stay byte-identical
//! (this module is a pure, on-demand producer — nothing touches the IR).
//!
//! OKF conformance (SPEC §9): every non-reserved `.md` carries parseable YAML
//! frontmatter with a non-empty `type`; reserved `index.md` files carry no
//! frontmatter except the bundle-root one, which may declare `okf_version`.
//!
//! Determinism: files are emitted by walking the tree (stable order) and the
//! chunk Vec (document order); the per-section chunk index is a `HashMap` used
//! only for lookup, never iterated for output. `timestamp` comes from the source
//! file's mtime (injected by the caller), never the wall clock — same source,
//! byte-identical bundle.

use crate::chunk::{self, Chunk, ChunkKind, ChunkOptions};
use crate::ir::{BBox, Document};
use crate::outline::{self, Section};
use std::collections::HashMap;
use std::path::PathBuf;

/// Export knobs (resource URI shaping + source identity). The caller supplies
/// `source_name`/`timestamp` from the file path so `core` stays IO-free.
#[derive(Debug, Clone, Default)]
pub struct OkfOptions {
    /// Prefix for the `resource` URI (e.g. `file:///data/docs/`). Default empty
    /// → the bare source basename, which keeps bundles byte-identical across
    /// machines.
    pub resource_base: String,
    /// Source document basename, used in `resource` URIs.
    pub source_name: String,
    /// ISO 8601 UTC timestamp (source mtime); `None` omits the field. NEVER the
    /// wall clock — that would break reproducibility.
    pub timestamp: Option<String>,
    /// Render tables as GitHub pipe tables in concept bodies (else tab/newline).
    pub table_markdown: bool,
}

/// An in-memory OKF bundle: relative paths → file contents, in emit order.
pub struct Bundle {
    pub files: Vec<(PathBuf, String)>,
}

impl Bundle {
    /// Write the bundle under `dir`, creating parent directories as needed.
    pub fn write_to(&self, dir: &std::path::Path) -> std::io::Result<()> {
        for (rel, content) in &self.files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, content)?;
        }
        Ok(())
    }

    /// Serialize to a POSIX **ustar** archive (for stdout / pipe / upload). All
    /// metadata is fixed (mode 0644, uid/gid 0, mtime 0, empty owner names) so
    /// the same bundle yields byte-identical tar bytes — the determinism
    /// guarantee extends to the archive, not just the on-disk directory. No
    /// `tar` crate dependency (those stamp the real mtime, breaking that).
    pub fn to_tar(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (rel, content) in &self.files {
            tar_entry(&mut out, &rel.to_string_lossy(), content.as_bytes());
        }
        // Two zero blocks mark end-of-archive.
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }
}

/// Append one regular-file ustar entry (512-byte header + padded data).
fn tar_entry(out: &mut Vec<u8>, path: &str, data: &[u8]) {
    let mut h = [0u8; 512];
    let (name, prefix) = tar_split_path(path);
    copy_field(&mut h[0..100], name.as_bytes());
    copy_field(&mut h[345..500], prefix.as_bytes());
    write_octal(&mut h[100..108], 0o644); // mode
    write_octal(&mut h[108..116], 0); // uid
    write_octal(&mut h[116..124], 0); // gid
    write_octal(&mut h[124..136], data.len() as u64); // size
    write_octal(&mut h[136..148], 0); // mtime = 0 (deterministic)
    h[156] = b'0'; // typeflag: regular file
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // Checksum: sum of all header bytes with the checksum field as spaces.
    for b in &mut h[148..156] {
        *b = b' ';
    }
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    // 6 octal digits + NUL + space (the conventional form).
    let cs = format!("{sum:06o}");
    h[148..154].copy_from_slice(cs.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    out.extend_from_slice(&h);
    out.extend_from_slice(data);
    let pad = (512 - data.len() % 512) % 512;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// Split a path into ustar (name ≤100, prefix ≤155) at a `/`. Falls back to a
/// truncated name for pathologically long paths (deeper than ustar supports).
fn tar_split_path(path: &str) -> (&str, &str) {
    if path.len() <= 100 {
        return (path, "");
    }
    // Find a split point so the suffix (name) fits in 100 and prefix in 155.
    if let Some(cut) = path
        .char_indices()
        .filter(|&(i, c)| c == '/' && path.len() - i - 1 <= 100 && i <= 155)
        .map(|(i, _)| i)
        .next()
    {
        (&path[cut + 1..], &path[..cut])
    } else {
        (&path[path.len() - 100..], "")
    }
}

/// Write a NUL-terminated, zero-padded octal numeric tar field.
fn write_octal(field: &mut [u8], val: u64) {
    let digits = field.len() - 1; // last byte stays NUL
    let s = format!("{val:0width$o}", width = digits);
    // If it overflows (shouldn't for our sizes), keep the low digits.
    let bytes = s.as_bytes();
    let start = bytes.len().saturating_sub(digits);
    field[..digits].copy_from_slice(&bytes[start..]);
    field[digits] = 0;
}

/// Copy `src` into a fixed-width tar field, truncating if needed (rest is NUL).
fn copy_field(field: &mut [u8], src: &[u8]) {
    let n = src.len().min(field.len());
    field[..n].copy_from_slice(&src[..n]);
}

/// Build an OKF bundle from a document. Joins the structure tree with chunks;
/// pure and deterministic.
pub fn build(doc: &Document, opts: &OkfOptions) -> Bundle {
    let root = outline::build(doc);
    let chunks = chunk::chunk_document_with(
        doc,
        ChunkOptions {
            table_markdown: opts.table_markdown,
            ..Default::default()
        },
    );
    // Group chunk indices by section id for O(1) lookup (never iterated for
    // output — emission order comes from the tree + chunk Vec).
    let mut by_section: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, c) in chunks.iter().enumerate() {
        by_section.entry(c.section_id).or_default().push(i);
    }
    let id_width = id_width(&root);
    let mut files = Vec::new();

    // Root-level content (before the first heading, or the whole body of an
    // untitled document) becomes a single Document concept so nothing is lost.
    if let Some(idxs) = by_section.get(&0) {
        let body_chunks: Vec<&Chunk> = idxs.iter().map(|&i| &chunks[i]).collect();
        if body_chunks.iter().any(|c| !c.text.trim().is_empty()) {
            let name = format!("{}-{}.md", pad(0, id_width), "document");
            let meta = ConceptMeta {
                typ: "Document",
                title: &doc_title(doc, opts),
                section_id: 0,
                page: body_page(&body_chunks),
                bbox: body_bbox(&body_chunks),
            };
            files.push((
                PathBuf::from(&name),
                concept_md(&meta, &body_chunks, opts, &[]),
            ));
        }
    }

    // Walk the tree, emitting one concept per section (and a dir index.md for
    // sections that have children).
    emit(
        &root,
        PathBuf::new(),
        &chunks,
        &by_section,
        id_width,
        opts,
        &mut files,
    );

    // Bundle-root index.md: okf_version + document metadata + top-level listing.
    files.push((
        PathBuf::from("index.md"),
        root_index_md(doc, &root, id_width, opts),
    ));

    Bundle { files }
}

/// Recursively emit concept files for a section's children under `dir`.
fn emit(
    section: &Section,
    dir: PathBuf,
    chunks: &[Chunk],
    by_section: &HashMap<usize, Vec<usize>>,
    id_width: usize,
    opts: &OkfOptions,
    files: &mut Vec<(PathBuf, String)>,
) {
    for child in &section.children {
        let stem = format!("{}-{}", pad(child.id, id_width), slug(&child.title));
        let body: Vec<&Chunk> = by_section
            .get(&child.id)
            .map(|idxs| idxs.iter().map(|&i| &chunks[i]).collect())
            .unwrap_or_default();
        let child_links = child_links(child, &dir, id_width);
        let meta = ConceptMeta {
            typ: "Section",
            title: &child.title,
            section_id: child.id,
            page: child.page,
            bbox: child.bbox,
        };
        files.push((
            dir.join(format!("{stem}.md")),
            concept_md(&meta, &body, opts, &child_links),
        ));
        if !child.children.is_empty() {
            let child_dir = dir.join(&stem);
            files.push((child_dir.join("index.md"), dir_index_md(child, id_width)));
            emit(child, child_dir, chunks, by_section, id_width, opts, files);
        }
    }
}

/// Identity of a concept (frontmatter scalars), grouped to keep `concept_md`'s
/// signature small.
struct ConceptMeta<'a> {
    typ: &'a str,
    title: &'a str,
    section_id: usize,
    page: usize,
    bbox: BBox,
}

/// One concept document: YAML frontmatter + a Markdown body. `links` are
/// absolute bundle paths to child concepts, appended as a "Subsections" list.
fn concept_md(
    meta: &ConceptMeta,
    body: &[&Chunk],
    opts: &OkfOptions,
    links: &[(String, String)],
) -> String {
    let ConceptMeta {
        typ,
        title,
        section_id,
        page,
        bbox,
    } = *meta;
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("type: {}\n", yaml_str(typ)));
    s.push_str(&format!("title: {}\n", yaml_str(title)));
    if page > 0 {
        s.push_str(&format!(
            "resource: {}\n",
            yaml_str(&resource_uri(opts, page, bbox))
        ));
    }
    if let Some(desc) = description(body) {
        s.push_str(&format!("description: {}\n", yaml_str(&desc)));
    }
    if let Some(ts) = &opts.timestamp {
        s.push_str(&format!("timestamp: {}\n", yaml_str(ts)));
    }
    // Extension keys (consumers must tolerate) for docparse-aware round-trips.
    s.push_str(&format!("section_id: {section_id}\n"));
    if page > 0 {
        s.push_str(&format!("page: {page}\n"));
        s.push_str(&format!(
            "bbox: [{}, {}, {}, {}]\n",
            bbox.x0, bbox.y0, bbox.x1, bbox.y1
        ));
    }
    s.push_str("---\n\n");

    s.push_str(&format!("# {title}\n\n"));
    // Body = the section's direct content chunks (its own heading is the title,
    // so skip it; sub-section content lives in their own files).
    for c in body {
        if c.kind == ChunkKind::Heading {
            continue;
        }
        s.push_str(&render_chunk(c));
        s.push_str("\n\n");
    }
    if !links.is_empty() {
        s.push_str("## Subsections\n\n");
        for (path, title) in links {
            s.push_str(&format!("- [{}]({})\n", title, path));
        }
        s.push('\n');
    }
    s
}

/// Markdown rendering of one chunk for a concept body.
fn render_chunk(c: &Chunk) -> String {
    match c.kind {
        ChunkKind::Code => format!("```\n{}\n```", c.text),
        ChunkKind::ListItem => format!("- {}", c.text),
        // Tables already arrive pipe/tab-rendered (ChunkOptions.table_markdown);
        // paragraphs pass through verbatim.
        _ => c.text.clone(),
    }
}

/// Bundle-root `index.md`: the sole index.md allowed frontmatter (okf_version),
/// plus document metadata and the top-level concept listing.
fn root_index_md(doc: &Document, root: &Section, id_width: usize, opts: &OkfOptions) -> String {
    // Use the basename (not the caller-set full path) so the bundle is
    // byte-identical across machines.
    let title = doc_title(doc, opts);
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str("okf_version: \"0.1\"\n");
    s.push_str(&format!("source: {}\n", yaml_str(&title)));
    s.push_str(&format!("pages: {}\n", doc.pages.len()));
    s.push_str(&format!("sections: {}\n", root.section_count()));
    s.push_str("---\n\n");
    s.push_str(&format!("# {title}\n\n"));
    s.push_str(&format!(
        "{} page(s), {} section(s).\n\n",
        doc.pages.len(),
        root.section_count()
    ));
    s.push_str("## Contents\n\n");
    for child in &root.children {
        let stem = format!("{}-{}", pad(child.id, id_width), slug(&child.title));
        s.push_str(&format!("- [{}](/{}.md)\n", child.title, stem));
    }
    s
}

/// A directory's `index.md` (non-root → no frontmatter): a heading + listing of
/// the section's children, with absolute bundle links.
fn dir_index_md(section: &Section, id_width: usize) -> String {
    let mut s = format!("# {}\n\n", section.title);
    for (path, title) in child_links(section, &PathBuf::new(), id_width) {
        s.push_str(&format!("- [{title}]({path})\n"));
    }
    s
}

/// Absolute bundle paths (`/a/b.md`) + titles for a section's child concepts.
/// `dir` is the section's own directory relative to the bundle root.
fn child_links(section: &Section, dir: &std::path::Path, id_width: usize) -> Vec<(String, String)> {
    let stem = format!("{}-{}", pad(section.id, id_width), slug(&section.title));
    let base = dir.join(&stem);
    section
        .children
        .iter()
        .map(|c| {
            let cstem = format!("{}-{}", pad(c.id, id_width), slug(&c.title));
            let rel = base.join(format!("{cstem}.md"));
            (format!("/{}", rel.to_string_lossy()), c.title.clone())
        })
        .collect()
}

/// `resource` URI: `<base><source>#page=<n>&bbox=<x0>,<y0>,<x1>,<y1>`.
fn resource_uri(opts: &OkfOptions, page: usize, b: BBox) -> String {
    format!(
        "{}{}#page={}&bbox={},{},{},{}",
        opts.resource_base, opts.source_name, page, b.x0, b.y0, b.x1, b.y1
    )
}

/// First sentence of the body (paragraph chunks), truncated — the recommended
/// one-sentence `description`. `None` when there's no prose to summarize.
fn description(body: &[&Chunk]) -> Option<String> {
    let text = body
        .iter()
        .find(|c| c.kind == ChunkKind::Paragraph)
        .map(|c| c.text.as_str())?;
    let sentence = text
        .split(['.', '。', '!', '?', '\n'])
        .next()
        .unwrap_or(text);
    let sentence = sentence.trim();
    if sentence.is_empty() {
        return None;
    }
    // Cap to keep it a "single sentence" and avoid runaway frontmatter.
    let capped: String = sentence.chars().take(200).collect();
    Some(capped)
}

fn doc_title(doc: &Document, opts: &OkfOptions) -> String {
    if !opts.source_name.is_empty() {
        opts.source_name.clone()
    } else {
        doc.source.clone()
    }
}

fn body_page(body: &[&Chunk]) -> usize {
    body.iter().map(|c| c.page).min().unwrap_or(0)
}

fn body_bbox(body: &[&Chunk]) -> BBox {
    body.iter().map(|c| c.bbox).fold(
        BBox {
            x0: f32::MAX,
            y0: f32::MAX,
            x1: f32::MIN,
            y1: f32::MIN,
        },
        |a, b| BBox {
            x0: a.x0.min(b.x0),
            y0: a.y0.min(b.y0),
            x1: a.x1.max(b.x1),
            y1: a.y1.max(b.y1),
        },
    )
}

/// Zero-padded section id for stable filename sorting.
fn pad(id: usize, width: usize) -> String {
    format!("{id:0width$}")
}

/// Digits needed for the largest section id in the tree (min 2).
fn id_width(root: &Section) -> usize {
    fn max_id(s: &Section) -> usize {
        s.children.iter().map(max_id).max().unwrap_or(0).max(s.id)
    }
    max_id(root).to_string().len().max(2)
}

/// Filename slug: drop a leading section number (the `NN-` prefix already
/// encodes order, so "1 Intro" → "intro" not "1-intro"), then keep
/// alphanumerics (incl. unicode letters/CJK), lowercase ASCII, collapse other
/// runs to single `-`, trim. Empty → "section".
fn slug(title: &str) -> String {
    let title = strip_leading_number(title);
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    // Cap length so a runaway "heading" (e.g. a misdetected paragraph) can't
    // produce a filename past the filesystem limit. The NN- prefix keeps names
    // unique regardless, so truncation only costs readability.
    const MAX_SLUG: usize = 64;
    let trimmed = out.trim_end_matches('-');
    let capped: String = trimmed.chars().take(MAX_SLUG).collect();
    let capped = capped.trim_end_matches('-');
    if capped.is_empty() {
        "section".to_string()
    } else {
        capped.to_string()
    }
}

/// Drop a leading section number like `1`, `2.1`, `3.2.4`, optionally with a
/// trailing dot/paren, plus following whitespace. Mirrors the bookmark matcher.
fn strip_leading_number(s: &str) -> &str {
    let rest = s.trim_start();
    let num_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    if num_end > 0 && rest[..num_end].contains(|c: char| c.is_ascii_digit()) {
        let after = rest[num_end..].trim_start_matches([')', '.']);
        if let Some(stripped) = after.strip_prefix(' ') {
            return stripped.trim_start();
        }
    }
    rest
}

/// Quote a YAML scalar (always double-quoted, escaping `\` and `"`) — keeps
/// titles with colons/quotes/unicode safe and parseable.
fn yaml_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Element, Page, TextChunk};

    fn text_el(t: &str, size: f32, y: f32) -> Element {
        Element::Text(TextChunk {
            text: t.into(),
            bbox: BBox {
                x0: 72.0,
                y0: y - size,
                x1: 520.0,
                y1: y,
            },
            font_size: size,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        })
    }

    fn doc(elements: Vec<Element>) -> Document {
        Document {
            source: "paper.pdf".into(),
            provenance: None,
            pages: vec![Page {
                number: 1,
                width: 612.0,
                height: 792.0,
                elements,
            }],
        }
    }

    fn opts() -> OkfOptions {
        OkfOptions {
            source_name: "paper.pdf".into(),
            timestamp: Some("2026-06-19T00:00:00Z".into()),
            ..Default::default()
        }
    }

    fn nested_doc() -> Document {
        doc(vec![
            text_el("1 Intro", 24.0, 740.0),
            text_el("Intro body sentence one. Second one.", 10.0, 712.0),
            text_el("1.1 Background", 16.0, 684.0),
            text_el("Background body text here.", 10.0, 656.0),
            text_el("2 Methods", 24.0, 628.0),
            text_el("Methods body text here.", 10.0, 600.0),
        ])
    }

    fn paths(b: &Bundle) -> Vec<String> {
        b.files
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn bundle_mirrors_the_tree_structure() {
        let b = build(&nested_doc(), &opts());
        let p = paths(&b);
        // Top-level Intro (with child) + its dir/index + child + Methods + root index.
        assert!(p.contains(&"01-intro.md".to_string()), "{p:?}");
        assert!(p.contains(&"01-intro/index.md".to_string()), "{p:?}");
        assert!(
            p.contains(&"01-intro/02-background.md".to_string()),
            "{p:?}"
        );
        assert!(p.contains(&"03-methods.md".to_string()), "{p:?}");
        assert!(p.contains(&"index.md".to_string()));
        // Concept count (excl. index.md files) == section_count.
        let concepts = b
            .files
            .iter()
            .filter(|(p, _)| !p.ends_with("index.md"))
            .count();
        assert_eq!(concepts, outline::build(&nested_doc()).section_count());
    }

    #[test]
    fn every_concept_has_frontmatter_with_nonempty_type() {
        // OKF §9 conformance self-check.
        let b = build(&nested_doc(), &opts());
        for (path, content) in &b.files {
            let is_index = path.file_name().and_then(|n| n.to_str()) == Some("index.md");
            if is_index {
                // Reserved: only the root index.md may carry frontmatter.
                if path.to_string_lossy() == "index.md" {
                    assert!(
                        content.contains("okf_version: \"0.1\""),
                        "root index okf_version"
                    );
                } else {
                    assert!(
                        !content.starts_with("---\n"),
                        "non-root index.md must have no frontmatter: {path:?}"
                    );
                }
                continue;
            }
            let fm = frontmatter(content).unwrap_or_else(|| panic!("no frontmatter: {path:?}"));
            let typ = fm
                .iter()
                .find(|(k, _)| k == "type")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            assert!(!typ.is_empty(), "non-empty type required: {path:?}");
        }
    }

    #[test]
    fn body_holds_only_direct_content_no_duplication() {
        let b = build(&nested_doc(), &opts());
        let intro = b
            .files
            .iter()
            .find(|(p, _)| p.to_string_lossy() == "01-intro.md")
            .unwrap();
        // Intro's own body, but NOT its subsection's body.
        assert!(intro.1.contains("Intro body sentence one"));
        assert!(
            !intro.1.contains("Background body text"),
            "child content must not leak into parent"
        );
        // The subsection link is present.
        assert!(intro.1.contains("/01-intro/02-background.md"));
    }

    #[test]
    fn deterministic_same_input_same_bytes() {
        let a = build(&nested_doc(), &opts());
        let c = build(&nested_doc(), &opts());
        assert_eq!(paths(&a), paths(&c));
        let ja: Vec<&String> = a.files.iter().map(|(_, c)| c).collect();
        let jc: Vec<&String> = c.files.iter().map(|(_, c)| c).collect();
        assert_eq!(ja, jc);
    }

    #[test]
    fn resource_is_citable_with_page_and_bbox() {
        let b = build(&nested_doc(), &opts());
        let intro = b
            .files
            .iter()
            .find(|(p, _)| p.to_string_lossy() == "01-intro.md")
            .unwrap();
        assert!(
            intro.1.contains("resource: \"paper.pdf#page=1&bbox="),
            "{}",
            intro.1
        );
    }

    #[test]
    fn untitled_document_degrades_to_root_concept() {
        let b = build(&doc(vec![text_el("just body text", 10.0, 740.0)]), &opts());
        let p = paths(&b);
        assert!(p.contains(&"index.md".to_string()));
        assert!(p.contains(&"00-document.md".to_string()), "{p:?}");
        let concept = b
            .files
            .iter()
            .find(|(p, _)| p.to_string_lossy() == "00-document.md")
            .unwrap();
        assert!(concept.1.contains("type: \"Document\""));
        assert!(concept.1.contains("just body text"));
    }

    #[test]
    fn no_wall_clock_timestamp() {
        // Omitting the mtime leaves no timestamp field at all.
        let mut o = opts();
        o.timestamp = None;
        let b = build(&nested_doc(), &o);
        for (_, content) in &b.files {
            assert!(
                !content.contains("timestamp:"),
                "no timestamp without mtime"
            );
        }
    }

    #[test]
    fn tar_is_deterministic_and_well_formed() {
        let b = build(&nested_doc(), &opts());
        let t1 = b.to_tar();
        let t2 = build(&nested_doc(), &opts()).to_tar();
        assert_eq!(t1, t2, "same bundle → byte-identical tar");
        // ustar: multiple of 512, ends with two zero blocks, magic present.
        assert_eq!(t1.len() % 512, 0);
        assert!(t1[t1.len() - 1024..].iter().all(|&b| b == 0));
        assert_eq!(&t1[257..263], b"ustar\0");
        // First entry's name is the first emitted file.
        let first = b.files[0].0.to_string_lossy();
        let name_end = t1[..100].iter().position(|&c| c == 0).unwrap_or(100);
        assert_eq!(&t1[..name_end], first.as_bytes());
        // mtime field is zero (deterministic, not wall clock).
        assert_eq!(&t1[136..147], b"00000000000");
    }

    #[test]
    fn slug_handles_unicode_and_punctuation() {
        assert_eq!(slug("3.2 Training Details!"), "training-details"); // leading number dropped
        assert_eq!(slug("引言"), "引言");
        assert_eq!(slug("***"), "section");
    }

    /// Minimal frontmatter parser for the conformance test: returns (key,value)
    /// pairs from the leading `---` block, or None if absent.
    fn frontmatter(content: &str) -> Option<Vec<(String, String)>> {
        let rest = content.strip_prefix("---\n")?;
        let end = rest.find("\n---\n")?;
        let block = &rest[..end];
        Some(
            block
                .lines()
                .filter_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
                })
                .collect(),
        )
    }
}
