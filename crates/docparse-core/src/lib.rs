//! docparse-core — format-agnostic core of docparse-rs.
//!
//! It defines:
//! - [`ir`] — the intermediate representation every parser produces.
//! - [`chunk`] — RAG chunking with chunk↔source-bbox citation.
//! - [`enhance`] — pluggable OCR/LLM boundary + per-page quality routing.
//! - [`parser`] — the [`parser::DocumentParser`] trait that PDF/DOCX/HTML
//!   backends implement.
//! - [`reading_order`] — a recursive XY-cut that linearizes a page.
//! - [`layout`] — lines → paragraphs/headings + header/footer detection.
//! - [`output`] — JSON / Markdown / plain-text serializers.
//! - [`table`] — bordered-table detection from ruling lines (semantic layer).
//! - [`quality`] — format-agnostic parse-quality scoring (coverage/garble).
//! - [`limits`] — resource guards (page count, zip-bomb pre-check) for the
//!   agent-facing entry points.
//!
//! The design mirrors the "structure extractor, not renderer" approach of
//! opendataloader-pdf: parsers emit positioned chunks; this crate turns them
//! into reading order and output without ever rasterizing.

pub mod chunk;
pub mod enhance;
pub mod ir;
pub mod layout;
pub mod limits;
pub mod output;
pub mod parser;
pub mod quality;
pub mod reading_order;
pub mod synth;
pub mod table;
pub mod table_cluster;
pub mod textio;
