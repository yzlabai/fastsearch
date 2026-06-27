//! Document structure tree (roadmap module 4, structure-tree iteration T1).
//!
//! A navigable hierarchy of [`Section`]s nested by heading level — the logical
//! outline of a long document (chapters → sections → subsections). This is what
//! agentic retrieval most wants beyond flat chunks: "list the table of contents,
//! drill into §3.2, read its subtree" instead of embedding a bag of chunks.
//!
//! The tree is **derived on demand** from the same reading-ordered blocks the
//! chunker uses ([`crate::layout::page_blocks`]) — it is NOT stored in the IR,
//! so `-f json` output stays byte-identical (the tree is its own `-f outline`).
//! Every block already carries a heading `level` (tagged PDFs supply H1..H6
//! directly; otherwise document-wide font-size tiers assign it — see
//! [`crate::layout`]), so the tree gets correct nesting for free, including
//! tagged documents. A level stack turns the flat heading sequence into nesting:
//! a heading pops ancestors at the same-or-deeper level, then attaches under the
//! nearest shallower one.
//!
//! Section ids are the heading's appearance order (root = 0, first heading = 1,
//! …). The chunker ([`crate::chunk`]) walks the same block order and tags each
//! chunk with the enclosing `section_id` using the identical scheme, so chunk
//! `section_id`s index straight into this tree (asserted by a cross-module test).

use crate::ir::{BBox, Document};
use crate::layout;
use serde::{Deserialize, Serialize};

/// A node in the document structure tree. The root is a synthetic document
/// node (`id` 0, `level` 0, empty `title`) whose `children` are the top-level
/// sections; every other node is a heading and the content beneath it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Section {
    /// Stable id = heading appearance order in reading order (root = 0). Chunks
    /// reference this via [`crate::chunk::Chunk::section_id`].
    pub id: usize,
    /// Heading text (empty for the synthetic root).
    pub title: String,
    /// Nesting level (1 = top-level section; 0 only for the root).
    pub level: u8,
    /// Page the heading sits on (1-based; 0 for the root).
    pub page: usize,
    /// Heading bounding box (PDF user space) — citation anchor. Zero for root.
    pub bbox: BBox,
    /// Child sections, in reading order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Section>,
}

const ZERO_BBOX: BBox = BBox {
    x0: 0.0,
    y0: 0.0,
    x1: 0.0,
    y1: 0.0,
};

/// Build the structure tree for a document. Always returns a (possibly
/// childless) synthetic root; a document with no detected headings yields a
/// root with no children.
pub fn build(doc: &Document) -> Section {
    // Arena: nodes[0] is the synthetic root. Children are stored as id lists so
    // the level-stack can attach nodes without fighting the borrow checker; the
    // arena is converted to the nested `Section` form at the end.
    struct Node {
        title: String,
        level: u8,
        page: usize,
        bbox: BBox,
        children: Vec<usize>,
    }
    let mut nodes: Vec<Node> = vec![Node {
        title: String::new(),
        level: 0,
        page: 0,
        bbox: ZERO_BBOX,
        children: Vec::new(),
    }];
    // Stack of ancestor ids, top = current section. Root (0) is the floor.
    let mut stack: Vec<usize> = vec![0];

    for blocks in layout::page_blocks(doc) {
        for b in blocks.iter().filter(|b| b.heading) {
            let level = b.level.max(1);
            // Pop ancestors at the same or deeper level — they're siblings or
            // descendants of this heading, not its parents.
            while stack.len() > 1 && nodes[*stack.last().unwrap()].level >= level {
                stack.pop();
            }
            let id = nodes.len();
            let parent = *stack.last().unwrap();
            nodes.push(Node {
                title: b.text.clone(),
                level,
                page: b.page,
                bbox: b.bbox,
                children: Vec::new(),
            });
            nodes[parent].children.push(id);
            stack.push(id);
        }
    }

    fn to_section(nodes: &[Node], id: usize) -> Section {
        let n = &nodes[id];
        Section {
            id,
            title: n.title.clone(),
            level: n.level,
            page: n.page,
            bbox: n.bbox,
            children: n.children.iter().map(|&c| to_section(nodes, c)).collect(),
        }
    }
    to_section(&nodes, 0)
}

impl Section {
    /// Find a section by id anywhere in the tree (root included).
    pub fn get(&self, id: usize) -> Option<&Section> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.get(id))
    }

    /// Ancestor titles from the outermost section down to (but excluding) the
    /// section with `id` — the heading breadcrumb. Empty if `id` is a top-level
    /// section or absent. Drives [`crate::chunk::Chunk::heading_path`] semantics.
    pub fn breadcrumb(&self, id: usize) -> Vec<String> {
        fn walk<'a>(node: &'a Section, id: usize, path: &mut Vec<&'a str>) -> bool {
            if node.id == id {
                return true;
            }
            for c in &node.children {
                // The root's empty title is never part of a breadcrumb.
                if node.id != 0 {
                    path.push(&node.title);
                }
                if walk(c, id, path) {
                    return true;
                }
                if node.id != 0 {
                    path.pop();
                }
            }
            false
        }
        let mut path = Vec::new();
        if walk(self, id, &mut path) {
            path.into_iter().map(String::from).collect()
        } else {
            Vec::new()
        }
    }

    /// Total section count (excluding the synthetic root).
    pub fn section_count(&self) -> usize {
        self.children.iter().map(|c| 1 + c.section_count()).sum()
    }

    /// A copy of this subtree with descendants beyond `max_depth` levels pruned
    /// (`0` = this node with no children; useful for "list the top-level table
    /// of contents" without serializing the whole document). `usize::MAX` keeps
    /// everything.
    pub fn pruned(&self, max_depth: usize) -> Section {
        Section {
            id: self.id,
            title: self.title.clone(),
            level: self.level,
            page: self.page,
            bbox: self.bbox,
            children: if max_depth == 0 {
                Vec::new()
            } else {
                self.children
                    .iter()
                    .map(|c| c.pruned(max_depth - 1))
                    .collect()
            },
        }
    }
}

/// Serialize the tree as pretty JSON (the `-f outline` payload).
pub fn to_json(root: &Section) -> String {
    serde_json::to_string_pretty(root).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Element, Page, TextChunk};

    /// One text element; `size` drives heading detection via font tiers.
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
            source: "t".into(),
            provenance: None,
            pages: vec![Page {
                number: 1,
                width: 612.0,
                height: 792.0,
                elements,
            }],
        }
    }

    #[test]
    fn nests_by_heading_level() {
        // H1(24) > body, H2(16) > body, H1(24) > body — three font tiers.
        let root = build(&doc(vec![
            text_el("1 Intro", 24.0, 740.0),
            text_el("intro body text here", 10.0, 710.0),
            text_el("1.1 Background", 16.0, 680.0),
            text_el("background body text", 10.0, 650.0),
            text_el("2 Methods", 24.0, 620.0),
            text_el("methods body text", 10.0, 590.0),
        ]));
        // Two top-level sections; the first has one subsection.
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].title, "1 Intro");
        assert_eq!(root.children[0].children.len(), 1);
        assert_eq!(root.children[0].children[0].title, "1.1 Background");
        assert_eq!(root.children[1].title, "2 Methods");
        assert!(root.children[1].children.is_empty());
        assert_eq!(root.section_count(), 3);
    }

    #[test]
    fn breadcrumb_walks_ancestors() {
        let root = build(&doc(vec![
            text_el("1 Intro", 24.0, 740.0),
            text_el("body", 10.0, 710.0),
            text_el("1.1 Background", 16.0, 680.0),
            text_el("body", 10.0, 650.0),
        ]));
        let sub = root.children[0].children[0].id;
        assert_eq!(root.breadcrumb(sub), vec!["1 Intro".to_string()]);
        let top = root.children[0].id;
        assert!(root.breadcrumb(top).is_empty());
    }

    #[test]
    fn ids_are_heading_appearance_order() {
        // Body at size 10 sets the font baseline so 24/16 read as headings.
        let root = build(&doc(vec![
            text_el("A", 24.0, 740.0),
            text_el("body one here", 10.0, 715.0),
            text_el("B", 16.0, 690.0),
            text_el("body two here", 10.0, 665.0),
            text_el("C", 24.0, 640.0),
            text_el("body three here", 10.0, 615.0),
        ]));
        // Root 0; headings get 1,2,3 in reading order regardless of nesting.
        assert_eq!(root.id, 0);
        assert_eq!(root.children[0].id, 1); // A
        assert_eq!(root.children[0].children[0].id, 2); // B (under A)
        assert_eq!(root.children[1].id, 3); // C
    }

    #[test]
    fn no_headings_yields_empty_root() {
        let root = build(&doc(vec![text_el("just body", 10.0, 740.0)]));
        assert_eq!(root.id, 0);
        assert!(root.children.is_empty());
        assert_eq!(root.section_count(), 0);
    }
}
