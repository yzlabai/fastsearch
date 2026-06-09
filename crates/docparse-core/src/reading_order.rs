//! Recursive XY-cut reading order.
//!
//! The classic algorithm: repeatedly split a region by the widest empty
//! horizontal band (into top/bottom), else the widest empty vertical band
//! (into columns), until a region can't be split, then sort it top-to-bottom,
//! left-to-right. This handles multi-column layouts that a naive y-then-x sort
//! would interleave. Coordinates are PDF space, so "top" = larger y.

use crate::ir::TextChunk;
use std::cmp::Ordering;

/// Returns indices into `chunks` in natural reading order.
pub fn reading_order(chunks: &[&TextChunk]) -> Vec<usize> {
    let idx: Vec<usize> = (0..chunks.len()).collect();
    let mut out = Vec::with_capacity(chunks.len());
    xy_cut(chunks, &idx, &mut out);
    out
}

fn xy_cut(chunks: &[&TextChunk], idx: &[usize], out: &mut Vec<usize>) {
    if idx.len() <= 1 {
        out.extend_from_slice(idx);
        return;
    }

    // Consider both a horizontal (row) cut and a vertical (column) cut, then
    // take whichever has the wider blank band. This is what makes column gutters
    // (typically much wider than inter-line gaps) win over row splits, so whole
    // columns are emitted before moving right.
    let h = horizontal_cut(chunks, idx);
    let v = vertical_cut(chunks, idx);
    let chosen = match (h, v) {
        (Some(h), Some(v)) => Some(if v.gap > h.gap { v } else { h }),
        (Some(h), None) => Some(h),
        (None, Some(v)) => Some(v),
        (None, None) => None,
    };
    if let Some(cut) = chosen {
        xy_cut(chunks, &cut.first, out);
        xy_cut(chunks, &cut.second, out);
        return;
    }

    // Base case: can't cut further — order within the region.
    let mut row = idx.to_vec();
    row.sort_by(|&a, &b| {
        let (ba, bb) = (chunks[a].bbox, chunks[b].bbox);
        cmp_desc(ba.cy(), bb.cy()).then(cmp_asc(ba.x0, bb.x0))
    });
    out.extend(row);
}

/// A chosen partition of indices into two parts, with the blank-band width that
/// justified it (so callers can compare horizontal vs vertical candidates).
struct Cut {
    gap: f32,
    first: Vec<usize>,
    second: Vec<usize>,
}

/// Split into (top, bottom) at the widest vertical gap, if one exceeds ~1.2×
/// the median line height (i.e. a clear blank band between rows of content).
fn horizontal_cut(chunks: &[&TextChunk], idx: &[usize]) -> Option<Cut> {
    let mut sorted = idx.to_vec();
    sorted.sort_by(|&a, &b| cmp_desc(chunks[a].bbox.y1, chunks[b].bbox.y1));

    let mut best_gap = 0.0f32;
    let mut best_at = 0usize;
    // `running_min_y` = lowest point of everything strictly above `i`.
    let mut running_min_y = chunks[sorted[0]].bbox.y0;
    for i in 1..sorted.len() {
        let top_next = chunks[sorted[i]].bbox.y1;
        let gap = running_min_y - top_next;
        if gap > best_gap {
            best_gap = gap;
            best_at = i;
        }
        running_min_y = running_min_y.min(chunks[sorted[i]].bbox.y0);
    }

    let threshold = median_height(chunks, idx) * 1.2;
    if best_at > 0 && best_gap > threshold {
        Some(Cut {
            gap: best_gap,
            first: sorted[..best_at].to_vec(),
            second: sorted[best_at..].to_vec(),
        })
    } else {
        None
    }
}

/// Split into (left, right) columns at the widest horizontal gap, if one
/// exceeds ~2× the median line height (column gutters are comparatively wide).
fn vertical_cut(chunks: &[&TextChunk], idx: &[usize]) -> Option<Cut> {
    let mut sorted = idx.to_vec();
    sorted.sort_by(|&a, &b| cmp_asc(chunks[a].bbox.x0, chunks[b].bbox.x0));

    let mut best_gap = 0.0f32;
    let mut best_at = 0usize;
    let mut running_max_x = chunks[sorted[0]].bbox.x1;
    for i in 1..sorted.len() {
        let left_next = chunks[sorted[i]].bbox.x0;
        let gap = left_next - running_max_x;
        if gap > best_gap {
            best_gap = gap;
            best_at = i;
        }
        running_max_x = running_max_x.max(chunks[sorted[i]].bbox.x1);
    }

    let threshold = median_height(chunks, idx) * 2.0;
    if best_at > 0 && best_gap > threshold {
        Some(Cut {
            gap: best_gap,
            first: sorted[..best_at].to_vec(),
            second: sorted[best_at..].to_vec(),
        })
    } else {
        None
    }
}

fn median_height(chunks: &[&TextChunk], idx: &[usize]) -> f32 {
    let mut hs: Vec<f32> = idx.iter().map(|&i| chunks[i].bbox.height()).collect();
    hs.sort_by(|a, b| cmp_asc(*a, *b));
    if hs.is_empty() {
        0.0
    } else {
        hs[hs.len() / 2]
    }
}

fn cmp_asc(a: f32, b: f32) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}
fn cmp_desc(a: f32, b: f32) -> Ordering {
    b.partial_cmp(&a).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::BBox;

    fn chunk(text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> TextChunk {
        TextChunk {
            text: text.into(),
            bbox: BBox { x0, y0, x1, y1 },
            font_size: (y1 - y0),
            font: None,
            page: 0,
            confidence: 1.0,
            bold: false,
        }
    }

    #[test]
    fn single_column_top_to_bottom() {
        let a = chunk("top", 0.0, 100.0, 50.0, 110.0);
        let b = chunk("bottom", 0.0, 10.0, 50.0, 20.0);
        let refs = vec![&b, &a]; // emitted out of order
        let order = reading_order(&refs);
        assert_eq!(refs[order[0]].text, "top");
        assert_eq!(refs[order[1]].text, "bottom");
    }

    #[test]
    fn two_columns_left_then_right() {
        // Left column (x ~0) should fully precede right column (x ~300).
        let l1 = chunk("L1", 0.0, 100.0, 50.0, 110.0);
        let l2 = chunk("L2", 0.0, 50.0, 50.0, 60.0);
        let r1 = chunk("R1", 300.0, 100.0, 350.0, 110.0);
        let r2 = chunk("R2", 300.0, 50.0, 350.0, 60.0);
        let refs = vec![&r1, &l2, &r2, &l1];
        let order = reading_order(&refs);
        let seq: Vec<&str> = order.iter().map(|&i| refs[i].text.as_str()).collect();
        assert_eq!(seq, vec!["L1", "L2", "R1", "R2"]);
    }
}
