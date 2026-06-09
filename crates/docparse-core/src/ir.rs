//! Intermediate representation produced by every parser.
//!
//! Coordinates are in PDF user space: origin bottom-left, y grows upward,
//! units are points (1/72 inch). Keeping a single coordinate convention across
//! formats lets reading-order and output stay format-agnostic.

use serde::{Deserialize, Serialize};

/// Axis-aligned bounding box in PDF user space (origin bottom-left).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
pub struct TextChunk {
    pub text: String,
    pub bbox: BBox,
    /// Effective font size in device space (already scaled by the text/CTM matrices).
    pub font_size: f32,
    /// Font resource name (e.g. "F1"), if known. TODO: resolve to PostScript name.
    pub font: Option<String>,
    pub page: usize,
}

/// A raster/vector image region. Position only for now (no pixel extraction yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageChunk {
    pub bbox: BBox,
    pub page: usize,
}

/// One element on a page. Semantic blocks (tables, lists, headings) are a
/// future layer built on top of these primitives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Element {
    Text(TextChunk),
    Image(ImageChunk),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub number: usize,
    pub width: f32,
    pub height: f32,
    pub elements: Vec<Element>,
}

impl Page {
    /// Borrow just the text chunks, in emission order.
    pub fn text_chunks(&self) -> Vec<&TextChunk> {
        self.elements
            .iter()
            .filter_map(|e| match e {
                Element::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Source path or identifier the document was parsed from.
    pub source: String,
    pub pages: Vec<Page>,
}
