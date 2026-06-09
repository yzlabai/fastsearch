//! Output serializers: JSON (full IR), Markdown, and plain text.
//!
//! Markdown/text first reconstruct *lines* from the per-glyph/per-run chunks a
//! parser emits, inserting word spaces by geometric gap (subset fonts often
//! emit one chunk per glyph, so we must not blindly space-join). This mirrors
//! how text extractors turn positioned glyphs back into readable words.

use crate::ir::{Page, TextChunk};
use crate::reading_order::reading_order;

/// Full IR as pretty JSON.
pub fn to_json(doc: &crate::ir::Document) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(doc)?)
}

/// Plain text in reading order, one reconstructed line per line.
pub fn to_text(doc: &crate::ir::Document) -> String {
    let mut s = String::new();
    for page in &doc.pages {
        for line in reconstruct_lines(page) {
            s.push_str(&line.text);
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

/// Markdown with a light heading heuristic (line font size ≥ 1.25× document
/// median becomes `##`). Tables/lists are a future semantic layer.
pub fn to_markdown(doc: &crate::ir::Document) -> String {
    let median = median_font_size(doc);

    let mut md = format!("<!-- source: {} -->\n\n", doc.source);
    for page in &doc.pages {
        for line in reconstruct_lines(page) {
            let t = line.text.trim();
            if t.is_empty() {
                continue;
            }
            if median > 0.0 && line.size > median * 1.25 {
                md.push_str("## ");
            }
            md.push_str(t);
            md.push_str("\n\n");
        }
    }
    md
}

/// A reconstructed text line.
struct Line {
    text: String,
    /// Representative (max) font size on the line — drives heading detection.
    size: f32,
}

/// Group reading-ordered chunks into lines (by shared baseline) and words
/// (by horizontal gap). A gap wider than ~0.25 em starts a new word.
fn reconstruct_lines(page: &Page) -> Vec<Line> {
    let chunks: Vec<&TextChunk> = page.text_chunks();
    let order = reading_order(&chunks);

    let mut lines: Vec<Line> = Vec::new();
    // Accumulator: (text, max_size, baseline_cy, last_x1).
    let mut cur: Option<(String, f32, f32, f32)> = None;

    for &i in &order {
        let c = chunks[i];
        let cy = c.bbox.cy();
        match cur.as_mut() {
            Some((text, size, line_cy, last_x1))
                if (*line_cy - cy).abs() <= c.font_size.max(1.0) * 0.5 =>
            {
                // Same line — insert a space only if there's a real gap.
                if c.bbox.x0 - *last_x1 > c.font_size * 0.25 {
                    text.push(' ');
                }
                text.push_str(&c.text);
                *last_x1 = c.bbox.x1;
                *size = size.max(c.font_size);
            }
            _ => {
                if let Some((text, size, _, _)) = cur.take() {
                    lines.push(Line { text, size });
                }
                cur = Some((c.text.clone(), c.font_size, cy, c.bbox.x1));
            }
        }
    }
    if let Some((text, size, _, _)) = cur {
        lines.push(Line { text, size });
    }
    lines
}

fn median_font_size(doc: &crate::ir::Document) -> f32 {
    let mut sizes: Vec<f32> = doc
        .pages
        .iter()
        .flat_map(|p| p.text_chunks().into_iter().map(|c| c.font_size))
        .collect();
    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sizes.is_empty() {
        0.0
    } else {
        sizes[sizes.len() / 2]
    }
}
