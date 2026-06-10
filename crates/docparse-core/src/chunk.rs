//! RAG chunking + chunk↔source citation (roadmap module 6).
//!
//! Splits a [`Document`] into retrieval-sized chunks that each carry their
//! source page + bbox and the enclosing heading breadcrumb. This is the feature
//! agents/RAG most want and that a black-box model pipeline gives only partly:
//! every chunk points back to exact coordinates ([`Chunk::page`]/[`Chunk::bbox`]),
//! and [`locate`] maps a coordinate back to its chunk — bidirectional citation.

use crate::ir::{BBox, Document, Element, Table};
use crate::layout::{self, Block};
use serde::{Deserialize, Serialize};

/// What kind of content a chunk holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Heading,
    Paragraph,
    Table,
}

/// A retrieval chunk with a precise source anchor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable sequential id within the document.
    pub id: usize,
    pub kind: ChunkKind,
    pub text: String,
    /// Source page (1-based, as in the IR).
    pub page: usize,
    /// Union bounding box of the source content on `page` (PDF user space).
    pub bbox: BBox,
    /// Enclosing heading breadcrumb, outermost first (section context).
    pub heading_path: Vec<String>,
    pub char_len: usize,
}

/// Chunking knobs.
#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    /// Soft target: accumulate consecutive paragraphs up to about this many
    /// characters before emitting a chunk.
    pub target_chars: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self { target_chars: 800 }
    }
}

/// Chunk a document with default options.
pub fn chunk_document(doc: &Document) -> Vec<Chunk> {
    chunk_document_with(doc, ChunkOptions::default())
}

/// Serialize chunks as pretty JSON (for CLI/observability).
pub fn to_json(chunks: &[Chunk]) -> String {
    serde_json::to_string_pretty(chunks).unwrap_or_default()
}

/// An item to emit, in reading order: a body block or a table.
enum Item<'a> {
    Block(&'a Block),
    Table(&'a Table),
}

pub fn chunk_document_with(doc: &Document, opts: ChunkOptions) -> Vec<Chunk> {
    let blocks_per_page = layout::page_blocks(doc);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut next_id = 0usize;
    // Heading breadcrumb stack: (font size, text).
    let mut headings: Vec<(f32, String)> = Vec::new();

    // Pending paragraph accumulator (single page).
    let mut buf: Option<ParaBuf> = None;

    let flush = |buf: &mut Option<ParaBuf>, chunks: &mut Vec<Chunk>, next_id: &mut usize| {
        if let Some(p) = buf.take() {
            chunks.push(Chunk {
                id: *next_id,
                kind: ChunkKind::Paragraph,
                text: p.text,
                page: p.page,
                bbox: p.bbox,
                heading_path: p.heading_path,
                char_len: p.char_len,
            });
            *next_id += 1;
        }
    };

    for (blocks, page) in blocks_per_page.iter().zip(&doc.pages) {
        let tables: Vec<&Table> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .collect();

        // Blocks arrive in reading order from layout (column-aware XY-cut). A
        // page-wide y-sort here would re-interleave two-column pages (left and
        // right columns share y ranges), so keep block order and splice each
        // table in before the first block that follows it within its own
        // column: horizontal overlap + top edge below the table's. Tables are
        // processed bottom-up so ones sharing an anchor end up top-to-bottom.
        let mut items: Vec<Item> = blocks.iter().map(Item::Block).collect();
        let mut tables_by_y = tables.clone();
        tables_by_y.sort_by(|a, b| {
            a.bbox
                .y1
                .partial_cmp(&b.bbox.y1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // "Follows in its column" = horizontal overlap + top edge below — for
        // previously spliced tables too, else a right-column table would jump
        // ahead of an unrelated left-column one on y alone.
        let follows =
            |bb: &BBox, t: &Table| bb.x0 < t.bbox.x1 && t.bbox.x0 < bb.x1 && bb.y1 < t.bbox.y1;
        for t in tables_by_y {
            let pos = items
                .iter()
                .position(|it| match it {
                    Item::Block(b) => follows(&b.bbox, t),
                    Item::Table(prev) => follows(&prev.bbox, t),
                })
                .unwrap_or(items.len());
            items.insert(pos, Item::Table(t));
        }

        for item in items {
            match item {
                Item::Block(b) if b.heading => {
                    flush(&mut buf, &mut chunks, &mut next_id);
                    // Update breadcrumb: pop same/again-deeper levels, push this.
                    while headings.last().is_some_and(|(s, _)| *s <= b.size) {
                        headings.pop();
                    }
                    let parent: Vec<String> = headings.iter().map(|(_, t)| t.clone()).collect();
                    headings.push((b.size, b.text.clone()));
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Heading,
                        text: b.text.clone(),
                        page: b.page,
                        bbox: b.bbox,
                        heading_path: parent,
                        char_len: b.text.chars().count(),
                    });
                    next_id += 1;
                }
                Item::Block(b) => {
                    let path: Vec<String> = headings.iter().map(|(_, t)| t.clone()).collect();
                    match buf.as_mut() {
                        // Continue accumulating within the same page.
                        Some(p) if p.page == b.page && p.char_len < opts.target_chars => {
                            p.push(b);
                        }
                        _ => {
                            flush(&mut buf, &mut chunks, &mut next_id);
                            buf = Some(ParaBuf::start(b, path));
                        }
                    }
                }
                Item::Table(t) => {
                    flush(&mut buf, &mut chunks, &mut next_id);
                    let path: Vec<String> = headings.iter().map(|(_, t)| t.clone()).collect();
                    let text = table_text(t);
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Table,
                        char_len: text.chars().count(),
                        text,
                        page: t.page,
                        bbox: t.bbox,
                        heading_path: path,
                    });
                    next_id += 1;
                }
            }
        }
        flush(&mut buf, &mut chunks, &mut next_id);
    }
    flush(&mut buf, &mut chunks, &mut next_id);
    chunks
}

/// Accumulates consecutive paragraph blocks on one page into a chunk.
struct ParaBuf {
    text: String,
    page: usize,
    bbox: BBox,
    heading_path: Vec<String>,
    char_len: usize,
}

impl ParaBuf {
    fn start(b: &Block, heading_path: Vec<String>) -> Self {
        Self {
            text: b.text.clone(),
            page: b.page,
            bbox: b.bbox,
            heading_path,
            char_len: b.text.chars().count(),
        }
    }
    fn push(&mut self, b: &Block) {
        self.text.push_str("\n\n");
        self.text.push_str(&b.text);
        self.char_len = self.text.chars().count();
        self.bbox = union(self.bbox, b.bbox);
    }
}

fn union(a: BBox, b: BBox) -> BBox {
    BBox {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

/// Tab/newline rendering of a table for a chunk's text.
fn table_text(t: &Table) -> String {
    t.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bidirectional citation: find the chunk whose source box on `page` contains
/// the point `(x, y)` (PDF user space). Returns the first match in id order.
pub fn locate(chunks: &[Chunk], page: usize, x: f32, y: f32) -> Option<&Chunk> {
    chunks.iter().find(|c| {
        c.page == page && x >= c.bbox.x0 && x <= c.bbox.x1 && y >= c.bbox.y0 && y <= c.bbox.y1
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BBox, Cell, Element, Page, TextChunk};

    fn text_el(t: &str, size: f32, y: f32, page: usize) -> Element {
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
            page,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
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
    fn heading_sets_breadcrumb_for_following_paragraph() {
        let d = doc(vec![
            text_el("Big Heading", 20.0, 700.0, 1),
            text_el("Some body paragraph text here.", 10.0, 660.0, 1),
        ]);
        let chunks = chunk_document(&d);
        let para = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Paragraph)
            .unwrap();
        assert_eq!(para.heading_path, vec!["Big Heading".to_string()]);
        assert_eq!(para.page, 1);
    }

    #[test]
    fn table_becomes_table_chunk_and_is_citable() {
        let table = Element::Table(Table {
            bbox: BBox {
                x0: 72.0,
                y0: 400.0,
                x1: 540.0,
                y1: 500.0,
            },
            page: 1,
            rows: vec![vec![
                Cell {
                    text: "A".into(),
                    bbox: BBox {
                        x0: 72.0,
                        y0: 450.0,
                        x1: 300.0,
                        y1: 500.0,
                    },
                },
                Cell {
                    text: "B".into(),
                    bbox: BBox {
                        x0: 300.0,
                        y0: 450.0,
                        x1: 540.0,
                        y1: 500.0,
                    },
                },
            ]],
        });
        let chunks = chunk_document(&doc(vec![table]));
        let t = chunks.iter().find(|c| c.kind == ChunkKind::Table).unwrap();
        assert!(t.text.contains("A\tB"));
        // bbox→chunk: a point inside the table resolves to it.
        let hit = locate(&chunks, 1, 100.0, 450.0).unwrap();
        assert_eq!(hit.id, t.id);
        // a point off the table resolves to nothing here.
        assert!(locate(&chunks, 1, 100.0, 50.0).is_none());
    }

    #[test]
    fn ids_are_stable_and_sequential() {
        let d = doc(vec![
            text_el("H", 20.0, 700.0, 1),
            text_el("para one text", 10.0, 660.0, 1),
        ]);
        let chunks = chunk_document(&d);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.id, i);
        }
    }
}
