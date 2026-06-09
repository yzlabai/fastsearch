//! docparse-core — format-agnostic core of docparse-rs.
//!
//! It defines:
//! - [`ir`] — the intermediate representation every parser produces.
//! - [`parser`] — the [`parser::DocumentParser`] trait that PDF/DOCX/HTML
//!   backends implement.
//! - [`reading_order`] — a recursive XY-cut that linearizes a page.
//! - [`output`] — JSON / Markdown / plain-text serializers.
//!
//! The design mirrors the "structure extractor, not renderer" approach of
//! opendataloader-pdf: parsers emit positioned chunks; this crate turns them
//! into reading order and output without ever rasterizing.

pub mod ir;
pub mod output;
pub mod parser;
pub mod reading_order;
