//! RAG chunking + chunk↔source citation (roadmap module 6).
//!
//! Splits a [`Document`] into retrieval-sized chunks that each carry their
//! source page + bbox and the enclosing heading breadcrumb. This is the feature
//! agents/RAG most want and that a black-box model pipeline gives only partly:
//! every chunk points back to exact coordinates ([`Chunk::page`]/[`Chunk::bbox`]),
//! and [`locate`] maps a coordinate back to its chunk — bidirectional citation.

use crate::ir::{BBox, Cell, Document, Element, ImageChunk, Table};
use crate::layout::{self, Block};
use serde::{Deserialize, Serialize};

/// Minimum fraction of the page an image must cover to become its own chunk.
/// Filters icons, rules, bullets and other decorative rasters so RAG isn't
/// flooded with non-content images. Mirrors the VLM figure gate
/// (`MIN_FIGURE_COVERAGE`) so the same images that get described also get
/// chunked.
const MIN_IMAGE_COVERAGE: f32 = 0.01;

/// Vertical gap (PDF points) within which an adjacent line/block is considered
/// to belong to an image — for caption matching and context extraction. About
/// three body lines; captions sit directly under/over a figure.
const IMAGE_ADJ_GAP: f32 = 40.0;

/// Max characters of surrounding-text context attached to an image chunk.
const IMAGE_CONTEXT_CHARS: usize = 300;

/// What kind of content a chunk holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Heading,
    Paragraph,
    Table,
    /// A monospace code block (line breaks + indentation preserved).
    Code,
    /// A list item (bullet/ordinal marker or LI tag).
    ListItem,
    /// A figure/picture. Its `text` carries the caption + surrounding context
    /// for retrieval; render/audit data lives in [`Chunk::image`].
    Image,
}

/// Render/audit payload for an [`ChunkKind::Image`] chunk. Kept separate from
/// the flat retrieval fields so non-image chunks serialize without it
/// (`skip_serializing_if`). The chunk's `text` already holds the searchable
/// caption+context; these fields let a consumer display and cite the image.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageMeta {
    /// Path the image was exported to (`--image-dir`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Base64 payload when embedded output was requested (`--image-embed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// MIME type of `data_base64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Figure caption alone (also folded into the chunk's `text`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// Where the caption came from (`"caption-line"`, `"vlm:<model>"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_source: Option<String>,
}

/// A retrieval chunk with a precise source anchor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    /// Id of the enclosing [`crate::outline::Section`] (0 = before any heading /
    /// document root). Lets a consumer map a chunk back into the structure tree
    /// for parent-document / auto-merging retrieval.
    #[serde(default)]
    pub section_id: usize,
    pub char_len: usize,
    /// Present only on [`ChunkKind::Image`] chunks: how to render and cite the
    /// figure. `None` for text/table chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageMeta>,
}

/// Chunking knobs.
#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    /// Soft target: accumulate consecutive paragraphs up to about this many
    /// characters before emitting a chunk.
    pub target_chars: usize,
    /// Table chunk text rendering: `false` = tab/newline (default, compact);
    /// `true` = GitHub pipe table (friendlier for markdown-native RAG consumers).
    pub table_markdown: bool,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            target_chars: 800,
            table_markdown: false,
        }
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

/// An item to emit, in reading order: a body block, a table, or an image.
enum Item<'a> {
    Block(&'a Block),
    Table(&'a Table),
    Image(&'a ImageChunk),
}

impl Item<'_> {
    fn bbox(&self) -> BBox {
        match self {
            Item::Block(b) => b.bbox,
            Item::Table(t) => t.bbox,
            Item::Image(i) => i.bbox,
        }
    }
}

/// "Follows `target` in its column": horizontal overlap + top edge below
/// `target`'s top. Used to splice floats (tables/images) into block reading
/// order without a right-column float jumping ahead of an unrelated left one
/// on `y` alone.
fn follows(bb: &BBox, target: &BBox) -> bool {
    bb.x0 < target.x1 && target.x0 < bb.x1 && bb.y1 < target.y1
}

pub fn chunk_document_with(doc: &Document, opts: ChunkOptions) -> Vec<Chunk> {
    let blocks_per_page = layout::page_blocks(doc);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut next_id = 0usize;
    // Section stack: (heading level, section id, title), outermost first. Uses
    // the real heading `level` (not font size) so the breadcrumb is correct even
    // when font sizes aren't monotonic with depth, and mirrors the id scheme in
    // `outline::build` (section id = heading appearance order) so a chunk's
    // `section_id` indexes straight into the structure tree.
    let mut sections: Vec<(u8, usize, String)> = Vec::new();
    let mut next_section = 1usize;
    let path_of = |sections: &[(u8, usize, String)]| -> Vec<String> {
        sections.iter().map(|(_, _, t)| t.clone()).collect()
    };
    let section_of = |sections: &[(u8, usize, String)]| -> usize {
        sections.last().map(|(_, id, _)| *id).unwrap_or(0)
    };

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
                section_id: p.section_id,
                char_len: p.char_len,
                image: None,
            });
            *next_id += 1;
        }
    };

    for (blocks, page) in blocks_per_page.iter().zip(&doc.pages) {
        let tables: Vec<&Table> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                // Skip empty-row placeholders (unfilled layout table regions).
                Element::Table(t) if !t.rows.is_empty() => Some(t),
                _ => None,
            })
            .collect();
        // Content images: gated by page coverage so icons/rules don't flood RAG.
        let page_area = (page.width * page.height).max(1.0);
        let images: Vec<&ImageChunk> = page
            .elements
            .iter()
            .filter_map(|e| match e {
                Element::Image(i) => (i.bbox.width() * i.bbox.height() / page_area
                    >= MIN_IMAGE_COVERAGE)
                    .then_some(i),
                _ => None,
            })
            .collect();

        // Blocks arrive in reading order from layout (column-aware XY-cut). A
        // page-wide y-sort here would re-interleave two-column pages (left and
        // right columns share y ranges), so keep block order and splice each
        // table in before the first block that follows it within its own
        // column: horizontal overlap + top edge below the table's. Tables are
        // processed bottom-up so ones sharing an anchor end up top-to-bottom.
        // Caption lines bound to a chunked image are folded into that image's
        // chunk — drop them from the prose flow so they aren't emitted twice.
        // (Only when the in-document caption is actually used, i.e. the image
        // has no enhancer/VLM caption.)
        let mut consumed_captions: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        for im in &images {
            if im.caption.is_none() {
                if let Some(idx) = find_caption_idx(blocks, im) {
                    consumed_captions.insert(idx);
                }
            }
        }
        let mut items: Vec<Item> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| !consumed_captions.contains(i))
            .map(|(_, b)| Item::Block(b))
            .collect();
        // Splice floats (tables, then images) into block reading order. Each is
        // inserted before the first item that follows it in its column; floats
        // are processed bottom-up (y1 ascending) so ones sharing an anchor end
        // up top-to-bottom.
        let mut tables_by_y = tables.clone();
        tables_by_y.sort_by(|a, b| {
            a.bbox
                .y1
                .partial_cmp(&b.bbox.y1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for t in tables_by_y {
            let pos = items
                .iter()
                .position(|it| follows(&it.bbox(), &t.bbox))
                .unwrap_or(items.len());
            items.insert(pos, Item::Table(t));
        }
        let mut images_by_y = images.clone();
        images_by_y.sort_by(|a, b| {
            a.bbox
                .y1
                .partial_cmp(&b.bbox.y1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for im in images_by_y {
            let pos = items
                .iter()
                .position(|it| follows(&it.bbox(), &im.bbox))
                .unwrap_or(items.len());
            items.insert(pos, Item::Image(im));
        }

        for item in items {
            match item {
                Item::Block(b) if b.list_item => {
                    // List items stay one chunk each (G9b) — never folded
                    // into prose paragraphs.
                    flush(&mut buf, &mut chunks, &mut next_id);
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::ListItem,
                        char_len: b.text.chars().count(),
                        text: b.text.clone(),
                        page: b.page,
                        bbox: b.bbox,
                        heading_path: path_of(&sections),
                        section_id: section_of(&sections),
                        image: None,
                    });
                    next_id += 1;
                }
                Item::Block(b) if b.code => {
                    // Code blocks are self-contained chunks — never merged
                    // into prose paragraphs (G8a).
                    flush(&mut buf, &mut chunks, &mut next_id);
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Code,
                        char_len: b.text.chars().count(),
                        text: b.text.clone(),
                        page: b.page,
                        bbox: b.bbox,
                        heading_path: path_of(&sections),
                        section_id: section_of(&sections),
                        image: None,
                    });
                    next_id += 1;
                }
                Item::Block(b) if b.heading => {
                    flush(&mut buf, &mut chunks, &mut next_id);
                    // Update the section stack by real heading level (mirrors
                    // outline::build): pop same/deeper ancestors, then push this
                    // heading as its own section. The breadcrumb is the ancestor
                    // titles *before* the push; the heading belongs to itself.
                    let level = b.level.max(1);
                    while sections.last().is_some_and(|(l, _, _)| *l >= level) {
                        sections.pop();
                    }
                    let parent = path_of(&sections);
                    let section_id = next_section;
                    next_section += 1;
                    sections.push((level, section_id, b.text.clone()));
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Heading,
                        text: b.text.clone(),
                        page: b.page,
                        bbox: b.bbox,
                        heading_path: parent,
                        section_id,
                        char_len: b.text.chars().count(),
                        image: None,
                    });
                    next_id += 1;
                }
                Item::Block(b) => {
                    match buf.as_mut() {
                        // Continue accumulating within the same page.
                        Some(p) if p.page == b.page && p.char_len < opts.target_chars => {
                            p.push(b);
                        }
                        _ => {
                            flush(&mut buf, &mut chunks, &mut next_id);
                            buf =
                                Some(ParaBuf::start(b, path_of(&sections), section_of(&sections)));
                        }
                    }
                }
                Item::Table(t) => {
                    flush(&mut buf, &mut chunks, &mut next_id);
                    let text = if opts.table_markdown {
                        table_text_markdown(t)
                    } else {
                        table_text(t)
                    };
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Table,
                        char_len: text.chars().count(),
                        text,
                        page: t.page,
                        bbox: t.bbox,
                        heading_path: path_of(&sections),
                        section_id: section_of(&sections),
                        image: None,
                    });
                    next_id += 1;
                }
                Item::Image(im) => {
                    flush(&mut buf, &mut chunks, &mut next_id);
                    // Caption priority: an enhancer-supplied caption (VLM) wins;
                    // otherwise look for an adjacent in-document caption line.
                    let (caption, caption_source) = match (&im.caption, &im.caption_source) {
                        (Some(c), src) => (Some(c.clone()), src.clone()),
                        _ => find_caption(blocks, im)
                            .map(|(c, s)| (Some(c), Some(s.to_string())))
                            .unwrap_or((None, None)),
                    };
                    let context = find_context(blocks, im);
                    let text = image_text(im.page, caption.as_deref(), context.as_deref());
                    chunks.push(Chunk {
                        id: next_id,
                        kind: ChunkKind::Image,
                        char_len: text.chars().count(),
                        text,
                        page: im.page,
                        bbox: im.bbox,
                        heading_path: path_of(&sections),
                        section_id: section_of(&sections),
                        image: Some(ImageMeta {
                            file: im.file.clone(),
                            data_base64: im.data_base64.clone(),
                            media_type: im.data_media_type.clone(),
                            caption,
                            caption_source,
                        }),
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
    section_id: usize,
    char_len: usize,
}

impl ParaBuf {
    fn start(b: &Block, heading_path: Vec<String>, section_id: usize) -> Self {
        Self {
            text: b.text.clone(),
            page: b.page,
            bbox: b.bbox,
            heading_path,
            section_id,
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

/// Whether a line reads as a figure caption ("Figure 3", "Fig. 2", "图3",
/// "Abbildung 1"). Figure-only by design — table/"表" captions belong to table
/// chunks, not images.
fn is_caption_line(text: &str) -> bool {
    let t = text.trim_start();
    let lower = t.to_ascii_lowercase();
    const PREFIXES: [&str; 4] = ["figure", "fig.", "fig ", "abbildung"];
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // CJK figure caption: 图/圖 directly followed by a digit or space.
    let mut chars = t.chars();
    matches!(chars.next(), Some('图' | '圖'))
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c.is_whitespace())
}

/// Horizontal overlap between a block and an image box.
fn h_overlap(b: &BBox, im: &BBox) -> bool {
    b.x0 < im.x1 && im.x0 < b.x1
}

/// Clear vertical gap (PDF points) between an image and a block; 0 if they
/// overlap vertically. (PDF y grows upward: `y1` is the top edge.)
fn v_gap(b: &BBox, im: &BBox) -> f32 {
    if b.y1 <= im.y0 {
        im.y0 - b.y1 // block sits below the image
    } else if b.y0 >= im.y1 {
        b.y0 - im.y1 // block sits above the image
    } else {
        0.0 // vertically overlapping
    }
}

/// Whether a block reads as this image's caption: a layout-model / tagged-PDF
/// `Caption` block (precise), or a "Figure N" text-pattern block (zero-model
/// fallback). Headings never count.
fn block_is_caption(b: &Block) -> bool {
    !b.heading && (b.caption || is_caption_line(&b.text))
}

/// Index into `blocks` of the adjacent in-document caption for an image, if any:
/// the nearest horizontally-overlapping caption block within [`IMAGE_ADJ_GAP`].
fn find_caption_idx(blocks: &[Block], im: &ImageChunk) -> Option<usize> {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.page == im.page && block_is_caption(b) && h_overlap(&b.bbox, &im.bbox))
        .map(|(i, b)| (v_gap(&b.bbox, &im.bbox), i))
        .filter(|(g, _)| *g <= IMAGE_ADJ_GAP)
        .min_by(|(g1, _), (g2, _)| g1.partial_cmp(g2).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, i)| i)
}

/// The adjacent in-document caption for an image: its text + provenance
/// (`"layout-caption"` for a model/tagged Caption block, else `"caption-line"`
/// for a "Figure N" text match).
fn find_caption(blocks: &[Block], im: &ImageChunk) -> Option<(String, &'static str)> {
    find_caption_idx(blocks, im).map(|i| {
        let source = if blocks[i].caption {
            "layout-caption"
        } else {
            "caption-line"
        };
        (truncate(blocks[i].text.trim(), IMAGE_CONTEXT_CHARS), source)
    })
}

/// Surrounding prose context for an image: adjacent horizontally-overlapping
/// body blocks (caption lines excluded — those are the caption), nearest first,
/// concatenated up to [`IMAGE_CONTEXT_CHARS`]. Lets "as shown in Fig. N" text
/// retrieve the figure even when the figure itself has no caption.
fn find_context(blocks: &[Block], im: &ImageChunk) -> Option<String> {
    let mut cands: Vec<(f32, &Block)> = blocks
        .iter()
        .filter(|b| {
            b.page == im.page && !b.heading && h_overlap(&b.bbox, &im.bbox) && !block_is_caption(b)
        })
        .map(|b| (v_gap(&b.bbox, &im.bbox), b))
        .filter(|(g, _)| *g <= IMAGE_ADJ_GAP)
        .collect();
    cands.sort_by(|(g1, _), (g2, _)| g1.partial_cmp(g2).unwrap_or(std::cmp::Ordering::Equal));
    if cands.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (_, b) in cands {
        let t = b.text.trim();
        if t.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
        if out.chars().count() >= IMAGE_CONTEXT_CHARS {
            break;
        }
    }
    (!out.is_empty()).then(|| truncate(&out, IMAGE_CONTEXT_CHARS))
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Retrieval text for an image chunk: caption and/or context, with a stable
/// placeholder when neither is available (an empty-text chunk is useless to a
/// vector index but the figure is still renderable/citable).
fn image_text(page: usize, caption: Option<&str>, context: Option<&str>) -> String {
    match (caption, context) {
        (Some(c), Some(cx)) => format!("{c}\n\n{cx}"),
        (Some(c), None) => c.to_string(),
        (None, Some(cx)) => cx.to_string(),
        (None, None) => format!("[image p{page}]"),
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

/// GitHub pipe-table rendering of a table for a chunk's text (first row =
/// header). Cell `|`/newline are escaped; ragged rows are padded to the widest
/// column count so the table stays parseable.
fn table_text_markdown(t: &Table) -> String {
    if t.rows.is_empty() {
        return String::new();
    }
    let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    let esc = |s: &str| s.trim().replace('|', "\\|").replace('\n', " ");
    let fmt_row = |row: &[Cell]| {
        let mut cells: Vec<String> = row.iter().map(|c| esc(&c.text)).collect();
        cells.resize(cols, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut lines = Vec::with_capacity(t.rows.len() + 1);
    lines.push(fmt_row(&t.rows[0]));
    lines.push(format!("| {} |", vec!["---"; cols].join(" | ")));
    for row in &t.rows[1..] {
        lines.push(fmt_row(row));
    }
    lines.join("\n")
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
    use crate::ir::{BBox, Cell, Element, ImageChunk, ImageKind, Page, TextChunk};

    /// An image element spanning `[x0,x1]×[y0,y1]` (PDF user space).
    fn image_el(bbox: BBox, page: usize) -> Element {
        Element::Image(ImageChunk {
            bbox,
            page,
            width_px: 100,
            height_px: 100,
            turns: 0,
            kind: ImageKind::None,
            data: Vec::new(),
            file: Some("assets/fig.png".into()),
            data_base64: None,
            data_media_type: None,
            caption: None,
            caption_source: None,
        })
    }

    /// A text element at an explicit bbox (full control over geometry).
    fn text_at(t: &str, bbox: BBox, page: usize) -> Element {
        Element::Text(TextChunk {
            text: t.into(),
            bbox,
            font_size: 10.0,
            font: None,
            page,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        })
    }

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
    fn table_markdown_option_renders_pipe_table() {
        let bb = BBox {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let cell = |s: &str| Cell {
            text: s.into(),
            bbox: bb,
            row_span: 1,
            col_span: 1,
            merged: false,
        };
        let table = Element::Table(Table {
            bbox: bb,
            page: 1,
            rows: vec![
                vec![cell("方法"), cell("准确率")],
                vec![cell("BM25"), cell("0.81")],
            ],
            source: None,
        });
        let d = doc(vec![table]);

        // markdown 选项 → 管道表
        let md = chunk_document_with(
            &d,
            ChunkOptions {
                table_markdown: true,
                ..Default::default()
            },
        );
        let t = md.iter().find(|c| c.kind == ChunkKind::Table).unwrap();
        assert!(t.text.contains("| 方法 | 准确率 |"), "got: {}", t.text);
        assert!(t.text.contains("| --- | --- |"));

        // 默认 → tab/换行（向后兼容）
        let def = chunk_document_with(&d, ChunkOptions::default());
        let t2 = def.iter().find(|c| c.kind == ChunkKind::Table).unwrap();
        assert!(t2.text.contains("方法\t准确率"), "got: {}", t2.text);
        assert!(!t2.text.contains('|'));
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
            source: None,
            rows: vec![vec![
                Cell {
                    text: "A".into(),
                    bbox: BBox {
                        x0: 72.0,
                        y0: 450.0,
                        x1: 300.0,
                        y1: 500.0,
                    },
                    row_span: 1,
                    col_span: 1,
                    merged: false,
                },
                Cell {
                    text: "B".into(),
                    bbox: BBox {
                        x0: 300.0,
                        y0: 450.0,
                        x1: 540.0,
                        y1: 500.0,
                    },
                    row_span: 1,
                    col_span: 1,
                    merged: false,
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
    fn section_ids_index_into_the_outline_tree() {
        use crate::outline;
        // Nested doc: H1 > body, H2 > body, H1 > body.
        let d = doc(vec![
            text_el("1 Intro", 24.0, 740.0, 1),
            text_el("intro body text here", 10.0, 712.0, 1),
            text_el("1.1 Background", 16.0, 684.0, 1),
            text_el("background body text", 10.0, 656.0, 1),
            text_el("2 Methods", 24.0, 628.0, 1),
            text_el("methods body text", 10.0, 600.0, 1),
        ]);
        let root = outline::build(&d);
        let chunks = chunk_document(&d);

        for c in &chunks {
            // Every chunk references a real node in the tree.
            let sec = root
                .get(c.section_id)
                .unwrap_or_else(|| panic!("chunk {} -> missing section {}", c.id, c.section_id));
            if c.kind == ChunkKind::Heading {
                // A heading IS its section: id matches, breadcrumb = ancestors.
                assert_eq!(sec.title, c.text);
                assert_eq!(c.heading_path, root.breadcrumb(c.section_id));
            } else if c.section_id != 0 {
                // Content sits under its section: path = ancestors + section title.
                let mut expected = root.breadcrumb(c.section_id);
                expected.push(sec.title.clone());
                assert_eq!(c.heading_path, expected, "chunk {} path", c.id);
            }
        }
        // The "background" paragraph is under the "1.1 Background" subsection.
        let bg = chunks
            .iter()
            .find(|c| c.text.contains("background body"))
            .unwrap();
        assert_eq!(
            bg.heading_path,
            vec!["1 Intro".to_string(), "1.1 Background".to_string()]
        );
    }

    #[test]
    fn image_becomes_chunk_with_caption_and_context() {
        // Layout (PDF y-up): context prose just above, a 400×200 figure, an
        // adjacent "Figure 1" caption just below it.
        let ctx_bb = BBox {
            x0: 72.0,
            y0: 660.0,
            x1: 472.0,
            y1: 672.0,
        };
        let img_bb = BBox {
            x0: 72.0,
            y0: 450.0,
            x1: 472.0,
            y1: 650.0,
        };
        let cap_bb = BBox {
            x0: 72.0,
            y0: 434.0,
            x1: 472.0,
            y1: 446.0,
        };
        let d = doc(vec![
            text_at("As shown below the pipeline has three stages", ctx_bb, 1),
            image_el(img_bb, 1),
            text_at("Figure 1: System architecture overview", cap_bb, 1),
        ]);
        let chunks = chunk_document(&d);
        let img = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Image)
            .expect("an image chunk is produced");
        let meta = img.image.as_ref().unwrap();
        assert_eq!(meta.file.as_deref(), Some("assets/fig.png"));
        assert_eq!(
            meta.caption.as_deref(),
            Some("Figure 1: System architecture overview")
        );
        assert_eq!(meta.caption_source.as_deref(), Some("caption-line"));
        // Retrieval text folds in caption + surrounding prose.
        assert!(
            img.text.contains("Figure 1"),
            "caption in text: {}",
            img.text
        );
        assert!(
            img.text.contains("three stages"),
            "context in text: {}",
            img.text
        );
        // The caption line is consumed by the image, not also emitted as prose.
        assert!(
            !chunks
                .iter()
                .any(|c| c.kind == ChunkKind::Paragraph && c.text.starts_with("Figure 1")),
            "caption must not double as a paragraph chunk"
        );
        // Citable: a point inside the figure resolves to the image chunk.
        assert_eq!(locate(&chunks, 1, 200.0, 500.0).unwrap().id, img.id);
    }

    #[test]
    fn tiny_image_is_below_coverage_gate() {
        // A 20×20 icon on a Letter page is well under 1% — no image chunk.
        let icon = BBox {
            x0: 72.0,
            y0: 700.0,
            x1: 92.0,
            y1: 720.0,
        };
        let d = doc(vec![image_el(icon, 1)]);
        let chunks = chunk_document(&d);
        assert!(chunks.iter().all(|c| c.kind != ChunkKind::Image));
    }

    #[test]
    fn vlm_caption_on_imagechunk_wins() {
        let img_bb = BBox {
            x0: 72.0,
            y0: 450.0,
            x1: 472.0,
            y1: 650.0,
        };
        let mut el = image_el(img_bb, 1);
        if let Element::Image(i) = &mut el {
            i.caption = Some("A bar chart of revenue by year.".into());
            i.caption_source = Some("vlm:test-model".into());
        }
        let chunks = chunk_document(&doc(vec![el]));
        let img = chunks.iter().find(|c| c.kind == ChunkKind::Image).unwrap();
        let meta = img.image.as_ref().unwrap();
        assert_eq!(meta.caption_source.as_deref(), Some("vlm:test-model"));
        assert!(img.text.contains("bar chart"));
    }

    #[test]
    fn layout_caption_region_binds_without_figure_prefix() {
        // A caption with NO "Figure N" text prefix, tagged `Caption` by the
        // layout model (or a tagged PDF). The text pattern wouldn't match it;
        // the Caption tag must bind it, with provenance "layout-caption".
        let img_bb = BBox {
            x0: 72.0,
            y0: 450.0,
            x1: 472.0,
            y1: 650.0,
        };
        let cap_bb = BBox {
            x0: 72.0,
            y0: 434.0,
            x1: 472.0,
            y1: 446.0,
        };
        let cap = Element::Text(TextChunk {
            text: "System overview diagram".into(),
            bbox: cap_bb,
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: Some("Caption".into()),
        });
        let chunks = chunk_document(&doc(vec![image_el(img_bb, 1), cap]));
        let img = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Image)
            .expect("image chunk");
        let meta = img.image.as_ref().unwrap();
        assert_eq!(meta.caption.as_deref(), Some("System overview diagram"));
        assert_eq!(meta.caption_source.as_deref(), Some("layout-caption"));
        // The caption is folded into the image, not emitted as its own prose.
        assert!(!chunks
            .iter()
            .any(|c| c.kind == ChunkKind::Paragraph && c.text.contains("System overview")));
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
