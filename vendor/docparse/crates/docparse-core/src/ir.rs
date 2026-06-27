//! Intermediate representation produced by every parser.
//!
//! Coordinates are in PDF user space: origin bottom-left, y grows upward,
//! units are points (1/72 inch). Keeping a single coordinate convention across
//! formats lets reading-order and output stay format-agnostic.

use serde::{Deserialize, Serialize};

// Every type below that appears in a serialized output also carries
// `#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]`, so the JSON
// Schema for the agent-facing contract is generated from these very types (one
// source of truth — see `crate::schema`). It compiles to nothing without the
// `schema` feature, so pure-library builds don't pull schemars.

/// Version of this IR schema. Bumped when the serialized shape changes so an
/// agent consuming the JSON can check compatibility. Semantic versioning.
pub const SCHEMA_VERSION: &str = "0.8.0";

/// Where a [`Document`] came from: which parser/version produced it, under
/// which schema. The agent-facing trust/repro anchor (one per document; an
/// element's *source location* is its own `bbox`+`page`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Provenance {
    pub schema_version: String,
    /// Producing parser, e.g. "pdf".
    pub parser: String,
    /// Producing parser/crate version.
    pub parser_version: String,
}

impl Provenance {
    pub fn new(parser: impl Into<String>, parser_version: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            parser: parser.into(),
            parser_version: parser_version.into(),
        }
    }
}

/// Default confidence for deterministic (non-model) extraction.
fn full_confidence() -> f32 {
    1.0
}

/// Axis-aligned bounding box in PDF user space (origin bottom-left).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BBox {
    pub fn width(&self) -> f32 {
        (self.x1 - self.x0).abs()
    }
    pub fn height(&self) -> f32 {
        (self.y1 - self.y0).abs()
    }
    /// Vertical center — used by reading order ("top" = larger y).
    pub fn cy(&self) -> f32 {
        (self.y0 + self.y1) / 2.0
    }
}

/// A run of text with a position. The atomic unit emitted by parsers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TextChunk {
    pub text: String,
    pub bbox: BBox,
    /// Effective font size in device space (already scaled by the text/CTM matrices).
    pub font_size: f32,
    /// Font resource name (e.g. "F1"), if known. TODO: resolve to PostScript name.
    pub font: Option<String>,
    pub page: usize,
    /// Extraction confidence in [0,1]. 1.0 for deterministic parsing; lower
    /// when a pluggable model (OCR/VLM) produced or corrected this chunk (M7).
    #[serde(default = "full_confidence")]
    pub confidence: f32,
    /// Whether the glyphs are bold (from the font weight). Helps heading
    /// detection when headings are body-size but bold.
    #[serde(default)]
    pub bold: bool,
    /// True when the text is invisible to a human reader (render mode Tr 3/7,
    /// off-page bbox, or sub-readable font size) — a prompt-injection vector
    /// for agents. Hidden chunks stay in the IR (flagged, auditable) but are
    /// excluded from rendered outputs via [`Page::text_chunks`] (N5a).
    #[serde(default)]
    pub hidden: bool,
    /// Which producer emitted this text: `None` = the deterministic parser;
    /// `Some("ocr:...")` etc. = an enhancer (N3). Element-level provenance so
    /// downstream can audit exactly which text came from a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Reading group (G2 layout enhancer / G9a tagged PDFs): when set, layout
    /// reconstruction orders groups by this id before running XY-cut *within*
    /// each group — a layout model or the author's structure tree dictates
    /// macro reading order while deterministic geometry rules inside groups.
    /// `None` = no grouping (sorts after all groups if mixed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u32>,
    /// Structure role from a tagged PDF's structure tree ("H1".."H6", "P",
    /// "LI", "TD", …) — author-declared semantics (G9a). `None` on untagged
    /// content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Pixel format of an extracted raster image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ImageKind {
    /// Recorded position-only (unsupported encoding or below the size gate).
    #[default]
    None,
    /// Raw 8-bit grayscale, `width_px * height_px` bytes.
    Gray8,
    /// Raw 8-bit RGB, `width_px * height_px * 3` bytes.
    Rgb8,
    /// Undecoded JPEG file bytes (DCTDecode passthrough).
    Jpeg,
    /// Already-encoded image file bytes in some other format (PNG/GIF/… — the
    /// shape DOCX/PPTX/HTML media arrive in). [`ImageChunk::data_media_type`]
    /// names the format; export/embed pass `data` through verbatim.
    Encoded,
}

/// A raster image region. `data` carries the pixel payload only for
/// page-covering images (scan candidates, the OCR-enhancer input) so memory
/// stays bounded on image-heavy digital documents — unless the backend was
/// asked to decode everything (image export). It is never serialized; the
/// JSON keeps bbox/dims for audit, plus `file` once exported to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageChunk {
    pub bbox: BBox,
    pub page: usize,
    #[serde(default)]
    pub width_px: u32,
    #[serde(default)]
    pub height_px: u32,
    /// Quarter-turns (90° clockwise) the pixel buffer must be rotated to
    /// match the on-page (viewing) orientation of `bbox` — non-zero when the
    /// page carries /Rotate or the image is placed with a rotated CTM (H2a).
    #[serde(default, skip_serializing_if = "u8_is_zero")]
    pub turns: u8,
    #[serde(default)]
    pub kind: ImageKind,
    #[serde(skip)]
    pub data: Vec<u8>,
    /// Path the image was exported to (`--image-dir`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Base64 payload when embedded output was requested (`--image-embed` /
    /// `images=embedded`); [`ImageChunk::data_media_type`] names the format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    /// MIME type of `data_base64` (`image/jpeg` passthrough or `image/png`
    /// for re-encoded raw bitmaps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_media_type: Option<String>,
    /// Figure caption / description — the text that makes this image findable
    /// in RAG retrieval. Filled deterministically from an adjacent caption line
    /// at chunking time, or by the VLM enhancer (`--vlm`) which writes a neural
    /// description here. `None` when no caption was found. See `caption_source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// Provenance of `caption`: `"caption-line"` (an adjacent in-document
    /// caption), `"vlm:<model>"` (neural description). Per the project's "近似
    /// 必须标注" rule, every non-deterministic/derived value names its source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption_source: Option<String>,
}

/// One cell of a [`Table`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Cell {
    pub text: String,
    pub bbox: BBox,
    /// Span semantics (G3-R follow-up): the ANCHOR cell (top-left) of a
    /// merged region carries its spans; covered positions stay materialized
    /// in the grid (flat row-major indexing remains valid — the eval/ODL
    /// convention) and are marked [`Cell::merged`]. Deterministic detectors
    /// emit plain 1×1 cells; only the table model produces real spans.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub row_span: u32,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub col_span: u32,
    /// True for a grid position covered by an earlier anchor's span (its
    /// text is the replicated anchor text).
    #[serde(default, skip_serializing_if = "is_false")]
    pub merged: bool,
}

fn one() -> u32 {
    1
}
#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires &T
fn is_one(v: &u32) -> bool {
    *v == 1
}
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(v: &bool) -> bool {
    !*v
}
#[allow(clippy::trivially_copy_pass_by_ref)]
fn u8_is_zero(v: &u8) -> bool {
    *v == 0
}

/// A detected table: a grid of cells bounded by ruling lines. Built by the
/// semantic layer ([`crate::table`]) from text chunks + vector segments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Table {
    pub bbox: BBox,
    pub page: usize,
    /// Row-major: `rows[r][c]`. All rows have the same length (column count).
    pub rows: Vec<Vec<Cell>>,
    /// `None` = deterministic detection; `"vlm:<model>"` = grid re-extracted
    /// by a model (`--vlm-tables`) — same audit convention as `TextChunk`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One element on a page. `Table` is the first semantic block; lists/headings
/// build on these primitives later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Element {
    Text(TextChunk),
    Image(ImageChunk),
    Table(Table),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Page {
    pub number: usize,
    pub width: f32,
    pub height: f32,
    pub elements: Vec<Element>,
}

impl Page {
    /// Borrow the *visible* text chunks, in emission order — the input to
    /// layout/output/chunking. Hidden text (see [`TextChunk::hidden`]) is
    /// excluded here so every rendered surface drops it; it stays in the
    /// serialized IR for audit.
    pub fn text_chunks(&self) -> Vec<&TextChunk> {
        self.elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) if !t.hidden => Some(t),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Document {
    /// Source path or identifier the document was parsed from.
    pub source: String,
    /// Producing parser/version + schema version. `default` keeps older JSON
    /// (pre-provenance) loadable.
    #[serde(default)]
    pub provenance: Option<Provenance>,
    pub pages: Vec<Page>,
}
