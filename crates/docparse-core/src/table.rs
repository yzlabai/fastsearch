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

// ---- borderless tables (alignment-based, no ruling lines) ----------------

const CELL_GAP_EM: f32 = 1.5; // gap (× font size) that separates columns
const MIN_BL_ROWS: usize = 3; // ≥ this many aligned rows to be a table
const MIN_BL_COLS: usize = 2;

/// A within-row cell: an x-span plus its source chunks.
struct Seg<'a> {
    x0: f32,
    x1: f32,
    chunks: Vec<&'a TextChunk>,
}
/// A reconstructed row of cells on one baseline.
struct Row<'a> {
    cy: f32,
    size: f32,
    segs: Vec<Seg<'a>>,
}

/// A "numeric" cell: among non-space chars, >40% are digits (table data).
fn is_numeric_cell(text: &str) -> bool {
    let (mut digits, mut total) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        if c.is_ascii_digit() {
            digits += 1;
        }
    }
    total > 0 && digits * 10 > total * 4
}

fn center_in(c: &TextChunk, b: &BBox) -> bool {
    let cx = (c.bbox.x0 + c.bbox.x1) / 2.0;
    let cy = c.bbox.cy();
    cx >= b.x0 && cx <= b.x1 && cy >= b.y0 && cy <= b.y1
}

/// Group chunks into baseline rows; segment each row into gap-separated cells.
fn build_rows<'a>(chunks: &[&'a TextChunk]) -> Vec<Row<'a>> {
    let mut idx: Vec<usize> = (0..chunks.len()).collect();
    let cmp = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    idx.sort_by(|&a, &b| {
        cmp(chunks[b].bbox.cy(), chunks[a].bbox.cy()) // top→bottom
            .then(cmp(chunks[a].bbox.x0, chunks[b].bbox.x0)) // left→right
    });

    let mut groups: Vec<Vec<&TextChunk>> = Vec::new();
    for &i in &idx {
        let c = chunks[i];
        match groups.last_mut() {
            Some(g) if (g[0].bbox.cy() - c.bbox.cy()).abs() <= c.font_size.max(1.0) * 0.5 => {
                g.push(c)
            }
            _ => groups.push(vec![c]),
        }
    }

    groups
        .into_iter()
        .map(|g| {
            let size = g.iter().map(|c| c.font_size).fold(0.0f32, f32::max);
            let cy = g.iter().map(|c| c.bbox.cy()).sum::<f32>() / g.len() as f32;
            let mut segs: Vec<Seg> = Vec::new();
            for c in g {
                match segs.last_mut() {
                    Some(s) if c.bbox.x0 - s.x1 <= CELL_GAP_EM * size.max(1.0) => {
                        s.x1 = s.x1.max(c.bbox.x1);
                        s.chunks.push(c);
                    }
                    _ => segs.push(Seg { x0: c.bbox.x0, x1: c.bbox.x1, chunks: vec![c] }),
                }
            }
            Row { cy, size, segs }
        })
        .collect()
}

/// Detect borderless tables by column alignment across consecutive rows.
/// Conservative: only emits a ≥3×≥2 grid whose rows are vertically contiguous
/// and whose cells align to stable columns — so prose/figures aren't tables.
pub fn detect_borderless_tables(chunks: &[&TextChunk], exclude: &[BBox]) -> Vec<Table> {
    let kept: Vec<&TextChunk> = chunks
        .iter()
        .copied()
        .filter(|c| !exclude.iter().any(|b| center_in(c, b)))
        .collect();
    let rows = build_rows(&kept);

    let mut tables = Vec::new();
    let mut region: Vec<usize> = Vec::new();
    let mut cols: Vec<f32> = Vec::new();

    let col_tol = |size: f32| (size * 0.6).max(5.0);

    let mut close = |region: &mut Vec<usize>, cols: &mut Vec<f32>| {
        if region.len() >= MIN_BL_ROWS && cols.len() >= MIN_BL_COLS {
            if let Some(t) = build_borderless(&rows, region, cols) {
                tables.push(t);
            }
        }
        region.clear();
        cols.clear();
    };

    for (i, row) in rows.iter().enumerate() {
        // Only rows with ≥2 cells participate; others break a region.
        if row.segs.len() < 2 {
            close(&mut region, &mut cols);
            continue;
        }
        let tol = col_tol(row.size);
        if region.is_empty() {
            region.push(i);
            cols = row.segs.iter().map(|s| s.x0).collect();
            continue;
        }
        let prev = &rows[*region.last().unwrap()];
        let gap = prev.cy - row.cy;
        let contiguous = gap > 0.0 && gap <= row.size.max(1.0) * 2.5;
        let aligned = row
            .segs
            .iter()
            .filter(|s| cols.iter().any(|&c| (c - s.x0).abs() <= tol))
            .count();
        let fits = contiguous && aligned >= 2 && aligned * 10 >= row.segs.len() * 6;
        if fits {
            region.push(i);
            // Merge any new left-edges as additional columns.
            for s in &row.segs {
                if !cols.iter().any(|&c| (c - s.x0).abs() <= tol) {
                    cols.push(s.x0);
                }
            }
        } else {
            close(&mut region, &mut cols);
            region.push(i);
            cols = row.segs.iter().map(|s| s.x0).collect();
        }
    }
    close(&mut region, &mut cols);
    tables
}

/// Build a [`Table`] from a finalized region (row indices) and column x-edges.
fn build_borderless(rows: &[Row], region: &[usize], cols: &[f32]) -> Option<Table> {
    let mut xs = cols.to_vec();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let ncols = xs.len();

    let mut out_rows: Vec<Vec<Cell>> = Vec::with_capacity(region.len());
    let mut x_min = f32::MAX;
    let mut x_max = f32::MIN;
    let (mut y_top, mut y_bot) = (f32::MIN, f32::MAX);
    let mut filled_cells = 0usize;

    for &ri in region {
        let row = &rows[ri];
        let half = row.size.max(1.0) / 2.0;
        y_top = y_top.max(row.cy + half);
        y_bot = y_bot.min(row.cy - half);
        let mut cells: Vec<Vec<&TextChunk>> = vec![Vec::new(); ncols];
        for s in &row.segs {
            // nearest column
            let ci = (0..ncols)
                .min_by(|&a, &b| {
                    (xs[a] - s.x0).abs().partial_cmp(&(xs[b] - s.x0).abs()).unwrap()
                })
                .unwrap_or(0);
            cells[ci].extend(&s.chunks);
            x_min = x_min.min(s.x0);
            x_max = x_max.max(s.x1);
        }
        let mut cell_row = Vec::with_capacity(ncols);
        for (ci, cs) in cells.into_iter().enumerate() {
            if !cs.is_empty() {
                filled_cells += 1;
            }
            let text = crate::layout::reconstruct_lines(&cs)
                .iter()
                .map(|l| l.text.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let x0 = xs[ci];
            let x1 = if ci + 1 < ncols { xs[ci + 1] } else { x_max.max(x0 + 1.0) };
            cell_row.push(Cell { text, bbox: BBox { x0, y0: row.cy - half, x1, y1: row.cy + half } });
        }
        out_rows.push(cell_row);
    }

    // Density gate: a real table fills a good fraction of its grid.
    let total = region.len() * ncols;
    if total == 0 || filled_cells * 2 < total {
        return None;
    }

    // Content gate — the key discriminator from multi-column page layout:
    // table cells are SHORT and often NUMERIC; prose "cells" (a column of body
    // text) are long sentences. Reject anything that reads like running text.
    let mut len_sum = 0usize;
    let mut numeric = 0usize;
    for row in &out_rows {
        for cell in row {
            if cell.text.is_empty() {
                continue;
            }
            len_sum += cell.text.chars().count();
            if is_numeric_cell(&cell.text) {
                numeric += 1;
            }
        }
    }
    let avg_len = len_sum as f32 / filled_cells.max(1) as f32;
    let num_frac = numeric as f32 / filled_cells.max(1) as f32;
    // Long cells → it's column layout / prose, not a table.
    if avg_len > 25.0 {
        return None;
    }
    // A 2–3 column grid needs numeric evidence; wide grids (≥4 cols) are
    // structurally table-like enough on their own.
    if ncols < 4 && num_frac < 0.15 {
        return None;
    }

    Some(Table {
        bbox: BBox { x0: x_min, y0: y_bot, x1: x_max, y1: y_top },
        page: rows[region[0]].segs[0].chunks[0].page,
        rows: out_rows,
    })
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

    // chunk at row baseline `cy` (font size 10), spanning [x0,x1].
    fn cc(text: &str, x0: f32, x1: f32, cy: f32) -> TextChunk {
        chunk(text, x0, cy - 5.0, x1, cy + 5.0)
    }

    #[test]
    fn borderless_aligned_grid_detected() {
        // 3 rows × 2 columns aligned at x0=10 and x0=60 (gap 30 > 1.5em).
        let cs: Vec<TextChunk> = vec![
            cc("a1", 10.0, 30.0, 100.0), cc("b1", 60.0, 80.0, 100.0),
            cc("a2", 10.0, 30.0, 88.0), cc("b2", 60.0, 80.0, 88.0),
            cc("a3", 10.0, 30.0, 76.0), cc("b3", 60.0, 80.0, 76.0),
        ];
        let refs: Vec<&TextChunk> = cs.iter().collect();
        let tables = detect_borderless_tables(&refs, &[]);
        assert_eq!(tables.len(), 1, "aligned grid is a table");
        assert_eq!(tables[0].rows.len(), 3);
        assert_eq!(tables[0].rows[0].len(), 2);
        assert_eq!(tables[0].rows[0][0].text, "a1");
        assert_eq!(tables[0].rows[2][1].text, "b3");
    }

    #[test]
    fn prose_is_not_a_borderless_table() {
        // Single wide run per line → one cell per row → not a table.
        let cs: Vec<TextChunk> = vec![
            cc("a line of ordinary prose text", 10.0, 200.0, 100.0),
            cc("another ordinary prose line here", 10.0, 210.0, 88.0),
            cc("and a third line of body text", 10.0, 205.0, 76.0),
        ];
        let refs: Vec<&TextChunk> = cs.iter().collect();
        assert!(detect_borderless_tables(&refs, &[]).is_empty());
    }

    #[test]
    fn borderless_skips_excluded_bordered_region() {
        let cs: Vec<TextChunk> = vec![
            cc("a1", 10.0, 30.0, 100.0), cc("b1", 60.0, 80.0, 100.0),
            cc("a2", 10.0, 30.0, 88.0), cc("b2", 60.0, 80.0, 88.0),
            cc("a3", 10.0, 30.0, 76.0), cc("b3", 60.0, 80.0, 76.0),
        ];
        let refs: Vec<&TextChunk> = cs.iter().collect();
        let exclude = [BBox { x0: 0.0, y0: 70.0, x1: 100.0, y1: 110.0 }];
        assert!(detect_borderless_tables(&refs, &exclude).is_empty(), "excluded region not re-detected");
    }
}
