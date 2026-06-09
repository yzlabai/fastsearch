//! Bordered-table detection: rebuild a grid from vector ruling lines and drop
//! text into cells (roadmap module 4, the TEDS entry point).
//!
//! Algorithm referenced from veraPDF-wcag-algs `TableBorderConsumer`
//! (cluster ruling lines into a lattice, then assign content to cells),
//! independently implemented. MVP scope: axis-aligned **bordered** tables with
//! a real ≥2×2 grid and an outer rectangle; no merged cells, no borderless
//! tables (those route to a model in M7). Conservative gating avoids mistaking
//! figure/equation rules for tables.

use crate::ir::{BBox, Cell, Table, TextChunk};

/// A vector line segment in PDF user space (already CTM-transformed by the
/// backend). Produced from content-stream path operators.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

const AXIS_EPS: f32 = 1.5; // max deviation to count as horizontal/vertical
const MIN_LEN: f32 = 8.0; // ignore tiny segments (dots, ticks)
const SNAP: f32 = 3.0; // cluster ruling coordinates within this many points

impl Segment {
    fn is_h(&self) -> bool {
        (self.y0 - self.y1).abs() <= AXIS_EPS && (self.x0 - self.x1).abs() > MIN_LEN
    }
    fn is_v(&self) -> bool {
        (self.x0 - self.x1).abs() <= AXIS_EPS && (self.y0 - self.y1).abs() > MIN_LEN
    }
}

/// A horizontal ruling: constant y, spanning [x_lo, x_hi].
struct HLine {
    y: f32,
    x_lo: f32,
    x_hi: f32,
}
/// A vertical ruling: constant x, spanning [y_lo, y_hi].
struct VLine {
    x: f32,
    y_lo: f32,
    y_hi: f32,
}

/// Cluster sorted values within `SNAP`, returning one representative (mean) per
/// cluster, ascending.
fn cluster(mut vals: Vec<f32>) -> Vec<f32> {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<f32> = Vec::new();
    let mut group: Vec<f32> = Vec::new();
    for v in vals {
        if group.last().is_some_and(|&g| v - g > SNAP) {
            out.push(group.iter().sum::<f32>() / group.len() as f32);
            group.clear();
        }
        group.push(v);
    }
    if !group.is_empty() {
        out.push(group.iter().sum::<f32>() / group.len() as f32);
    }
    out
}

/// Detect bordered tables on one page from its text chunks and ruling segments.
pub fn detect_tables(chunks: &[&TextChunk], segments: &[Segment], page: usize) -> Vec<Table> {
    let hlines: Vec<HLine> = segments
        .iter()
        .filter(|s| s.is_h())
        .map(|s| HLine { y: (s.y0 + s.y1) / 2.0, x_lo: s.x0.min(s.x1), x_hi: s.x0.max(s.x1) })
        .collect();
    let vlines: Vec<VLine> = segments
        .iter()
        .filter(|s| s.is_v())
        .map(|s| VLine { x: (s.x0 + s.x1) / 2.0, y_lo: s.y0.min(s.y1), y_hi: s.y0.max(s.y1) })
        .collect();

    let row_ys = cluster(hlines.iter().map(|h| h.y).collect());
    let col_xs = cluster(vlines.iter().map(|v| v.x).collect());

    // Need a real grid: ≥2 row bands and ≥2 col bands (≥3 lines each way).
    if row_ys.len() < 3 || col_xs.len() < 3 {
        return Vec::new();
    }

    let left = *col_xs.first().unwrap();
    let right = *col_xs.last().unwrap();
    let bottom = *row_ys.first().unwrap();
    let top = *row_ys.last().unwrap();
    let width = right - left;
    let height = top - bottom;
    if width < MIN_LEN || height < MIN_LEN {
        return Vec::new();
    }

    // Outer rectangle must exist: top & bottom rules spanning ≥80% width, and
    // left & right rules spanning ≥80% height. Rejects scattered figure rules.
    let spans_w = |y: f32| {
        hlines.iter().any(|h| {
            (h.y - y).abs() <= SNAP && (h.x_hi - h.x_lo) >= width * 0.8
        })
    };
    let spans_h = |x: f32| {
        vlines.iter().any(|v| {
            (v.x - x).abs() <= SNAP && (v.y_hi - v.y_lo) >= height * 0.8
        })
    };
    if !(spans_w(top) && spans_w(bottom) && spans_h(left) && spans_h(right)) {
        return Vec::new();
    }

    // Build the grid. Rows top→bottom (descending y), cols left→right.
    let mut ys = row_ys.clone();
    ys.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal)); // desc
    let xs = col_xs; // asc

    let n_rows = ys.len() - 1;
    let n_cols = xs.len() - 1;
    let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(n_rows);
    for r in 0..n_rows {
        let y_top = ys[r];
        let y_bot = ys[r + 1];
        let mut row: Vec<Cell> = Vec::with_capacity(n_cols);
        for c in 0..n_cols {
            let x_left = xs[c];
            let x_right = xs[c + 1];
            // Chunks whose center falls in this cell. Reuse line/word
            // reconstruction so per-glyph chunks form words (not "C O N T O")
            // and a multi-line cell keeps its lines in order.
            let cell_chunks: Vec<&TextChunk> = chunks
                .iter()
                .copied()
                .filter(|t| {
                    let cx = (t.bbox.x0 + t.bbox.x1) / 2.0;
                    let cy = t.bbox.cy();
                    cx >= x_left && cx <= x_right && cy >= y_bot && cy <= y_top
                })
                .collect();
            let text = crate::layout::reconstruct_lines(&cell_chunks)
                .iter()
                .map(|l| l.text.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            row.push(Cell {
                text,
                bbox: BBox { x0: x_left, y0: y_bot, x1: x_right, y1: y_top },
            });
        }
        rows.push(row);
    }

    vec![Table {
        bbox: BBox { x0: left, y0: bottom, x1: right, y1: top },
        page,
        rows,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BBox;

    fn h(y: f32, x0: f32, x1: f32) -> Segment {
        Segment { x0, y0: y, x1, y1: y }
    }
    fn v(x: f32, y0: f32, y1: f32) -> Segment {
        Segment { x0: x, y0, x1: x, y1 }
    }
    fn chunk(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> TextChunk {
        TextChunk {
            text: text.into(),
            bbox: BBox { x0, y0, x1, y1 },
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
        }
    }

    #[test]
    fn detects_2x2_grid_and_assigns_text() {
        // 3 horizontal rules (y=0,10,20) and 3 vertical (x=0,10,20) → 2×2.
        let segs = vec![
            h(0.0, 0.0, 20.0), h(10.0, 0.0, 20.0), h(20.0, 0.0, 20.0),
            v(0.0, 0.0, 20.0), v(10.0, 0.0, 20.0), v(20.0, 0.0, 20.0),
        ];
        let cs = [
            chunk("A", 1.0, 11.0, 4.0, 18.0),  // top-left
            chunk("B", 11.0, 11.0, 14.0, 18.0), // top-right
            chunk("C", 1.0, 1.0, 4.0, 8.0),     // bottom-left
            chunk("D", 11.0, 1.0, 14.0, 8.0),   // bottom-right
        ];
        let refs: Vec<&TextChunk> = cs.iter().collect();
        let tables = detect_tables(&refs, &segs, 1);
        assert_eq!(tables.len(), 1);
        let t = &tables[0];
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0].len(), 2);
        assert_eq!(t.rows[0][0].text, "A"); // top-left
        assert_eq!(t.rows[0][1].text, "B");
        assert_eq!(t.rows[1][0].text, "C"); // bottom-left
        assert_eq!(t.rows[1][1].text, "D");
    }

    #[test]
    fn scattered_rules_are_not_a_table() {
        // A few unrelated lines that don't form a closed grid.
        let segs = vec![h(0.0, 0.0, 50.0), h(30.0, 10.0, 15.0), v(5.0, 0.0, 4.0)];
        assert!(detect_tables(&[], &segs, 1).is_empty());
    }

    #[test]
    fn open_grid_without_outer_box_rejected() {
        // 3×3 short rules that don't span the full width/height → no outer box.
        let segs = vec![
            h(0.0, 0.0, 5.0), h(10.0, 0.0, 5.0), h(20.0, 0.0, 5.0),
            v(0.0, 0.0, 5.0), v(10.0, 0.0, 5.0), v(20.0, 0.0, 5.0),
        ];
        assert!(detect_tables(&[], &segs, 1).is_empty());
    }
}
