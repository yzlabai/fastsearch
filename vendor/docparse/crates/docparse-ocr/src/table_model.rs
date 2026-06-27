//! `--table-model` (G3-R R3): re-extract detected tables' structure with the
//! embedded UniRec model. Same discipline as `--vlm-tables`: render the
//! table's region on demand, ask the model, and REPLACE the deterministic
//! grid only when the answer parses into a real (≥2×2) grid — any failure
//! keeps the deterministic result. The win over the geometric detectors is
//! topology: UniRec emits HTML with `rowspan`/`colspan`, which we expand by
//! replicating the value into every spanned position (the same convention as
//! the eval pipeline and ODL's grid output).

use crate::unirec::UniRec;
use anyhow::Result;
use docparse_core::ir::{BBox, Cell, Document, Element, Table};

/// Render scale for table regions (pixels per PDF point).
const RENDER_SCALE: f32 = 3.0;
/// Generation cap — large tables run ~1000 tokens; runaway output is cut.
const MAX_TOKENS: usize = 2000;

/// Re-extract every detected table with UniRec. Returns the number replaced.
/// Per-table failures are reported on stderr and skipped (deterministic grid
/// stands), mirroring the VLM task's contract.
pub fn refine_tables(doc: &mut Document, pdf_bytes: Vec<u8>, model: &UniRec) -> Result<usize> {
    let raster = docparse_raster::Rasterizer::new(pdf_bytes)?;
    let mut refined = 0usize;
    for page in &mut doc.pages {
        let has_tables = page.elements.iter().any(|e| matches!(e, Element::Table(_)));
        if !has_tables {
            continue;
        }
        let (w, h, rgb) = match raster.render_rgb(page.number.saturating_sub(1), RENDER_SCALE) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("table-model: render failed on page {}: {e:#}", page.number);
                continue;
            }
        };
        for el in &mut page.elements {
            let Element::Table(table) = el else { continue };
            let Some((cw, ch, crop)) = crop_region(
                &rgb,
                w as usize,
                h as usize,
                &table.bbox,
                page.height,
                RENDER_SCALE,
            ) else {
                continue;
            };
            match model.recognize(&crop, cw, ch, MAX_TOKENS) {
                Ok(text) if crate::unirec::looks_degenerate(&text) => {
                    eprintln!(
                        "table-model: page {} degenerate answer; keeping deterministic rows",
                        page.number
                    );
                }
                Ok(text) => {
                    let Some(grid) = parse_html_table(&text) else {
                        eprintln!(
                            "table-model: page {} answer is not a usable table; keeping deterministic rows",
                            page.number
                        );
                        continue;
                    };
                    table.rows = grid_cells(&grid, &table.bbox);
                    table.source = Some("table:unirec-0.1b".to_string());
                    refined += 1;
                }
                Err(e) => eprintln!(
                    "table-model: inference failed on page {}: {e:#}",
                    page.number
                ),
            }
        }
    }
    // Note: empty-row placeholders the model couldn't fill are cleaned up by
    // the caller after all enhancers (covers --layout without --table-model too).
    Ok(refined)
}

/// Crop a PDF-space bbox (with a small margin) out of a page render.
pub(crate) fn crop_region(
    rgb: &[u8],
    w: usize,
    h: usize,
    bbox: &BBox,
    page_h: f32,
    scale: f32,
) -> Option<(usize, usize, Vec<u8>)> {
    const MARGIN_PT: f32 = 2.0;
    let x0 = (((bbox.x0 - MARGIN_PT) * scale).max(0.0) as usize).min(w);
    let x1 = (((bbox.x1 + MARGIN_PT) * scale) as usize).min(w);
    let y0 = (((page_h - bbox.y1 - MARGIN_PT) * scale).max(0.0) as usize).min(h);
    let y1 = (((page_h - bbox.y0 + MARGIN_PT) * scale) as usize).min(h);
    let (cw, ch) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if cw < 32 || ch < 32 {
        return None;
    }
    let mut out = vec![0u8; cw * ch * 3];
    for y in 0..ch {
        let src = ((y0 + y) * w + x0) * 3;
        out[y * cw * 3..(y + 1) * cw * 3].copy_from_slice(&rgb[src..src + cw * 3]);
    }
    Some((cw, ch, out))
}

/// A parsed grid position: the anchor of a merged region carries its spans;
/// covered positions replicate the text and are marked `merged` (flat
/// row-major indexing stays valid — the eval/ODL convention).
#[derive(Clone, Debug)]
pub struct ParsedCell {
    pub text: String,
    pub row_span: u32,
    pub col_span: u32,
    pub merged: bool,
}

/// Parse the model's HTML table subset (`<table>/<tr>/<td rowspan colspan>`,
/// `<th>` accepted) into a rectangular grid with spans EXPANDED — the
/// spanned value is replicated into every covered position, anchors keep
/// their span counts. Returns `None` unless the result is a real ≥2×2 grid
/// with some content (a prose answer must never replace a detected table).
pub fn parse_html_table(text: &str) -> Option<Vec<Vec<ParsedCell>>> {
    let start = text.find("<table")?;
    let body = &text[start..];
    let end = body.find("</table>").map(|i| i + 8).unwrap_or(body.len());
    let body = &body[..end];

    // Row-major construction with a pending-rowspan grid: pending[c] holds
    // (remaining_rows, text) for cells spanning down into the current row.
    let mut rows: Vec<Vec<ParsedCell>> = Vec::new();
    let mut pending: Vec<(usize, String)> = Vec::new();
    let covered = |text: &str| ParsedCell {
        text: text.to_string(),
        row_span: 1,
        col_span: 1,
        merged: true,
    };

    for tr in split_tags(body, "tr") {
        let mut row: Vec<ParsedCell> = Vec::new();
        let mut col = 0usize;
        let mut cells = split_cells(&tr).into_iter();
        loop {
            // Fill positions owed to earlier rowspans first.
            if let Some((left, t)) = pending.get_mut(col).filter(|(l, _)| *l > 0) {
                *left -= 1;
                let t = t.clone();
                row.push(covered(&t));
                col += 1;
                continue;
            }
            let Some((attrs, content)) = cells.next() else {
                break;
            };
            let rs = attr_usize(&attrs, "rowspan").max(1);
            let cs = attr_usize(&attrs, "colspan").max(1);
            let text = strip_tags(&content);
            for k in 0..cs {
                if pending.len() <= col {
                    pending.resize(col + 1, (0, String::new()));
                }
                pending[col] = (rs - 1, text.clone());
                row.push(if k == 0 {
                    ParsedCell {
                        text: text.clone(),
                        row_span: rs as u32,
                        col_span: cs as u32,
                        merged: false,
                    }
                } else {
                    covered(&text)
                });
                col += 1;
            }
        }
        // Trailing rowspan positions after the last explicit cell.
        while col < pending.len() {
            if pending[col].0 > 0 {
                pending[col].0 -= 1;
                let t = pending[col].1.clone();
                row.push(covered(&t));
            }
            col += 1;
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }

    let ncols = rows.iter().map(Vec::len).max()?;
    if rows.len() < 2 || ncols < 2 {
        return None;
    }
    if !rows.iter().flatten().any(|c| !c.text.trim().is_empty()) {
        return None;
    }
    let pad = ParsedCell {
        text: String::new(),
        row_span: 1,
        col_span: 1,
        merged: false,
    };
    for r in &mut rows {
        r.resize(ncols, pad.clone());
    }
    Some(rows)
}

/// Contents of every `<tag ...>...</tag>` occurrence, in order.
fn split_tags(body: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(gt) = after.find('>') else { break };
        let content = &after[gt + 1..];
        let end = content.find(&close).unwrap_or(content.len());
        out.push(content[..end].to_string());
        rest = &content[end..];
    }
    out
}

/// `<td attrs>content</td>` / `<th>` pairs within one row.
fn split_cells(tr: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = tr;
    loop {
        let (i, taglen) = match (rest.find("<td"), rest.find("<th")) {
            (Some(a), Some(b)) => (a.min(b), 3),
            (Some(a), None) => (a, 3),
            (None, Some(b)) => (b, 3),
            (None, None) => break,
        };
        let after = &rest[i + taglen..];
        let Some(gt) = after.find('>') else { break };
        let attrs = after[..gt].to_string();
        let content = &after[gt + 1..];
        let end = content
            .find("</td>")
            .or_else(|| content.find("</th>"))
            .or_else(|| content.find("<td"))
            .or_else(|| content.find("<tr"))
            .unwrap_or(content.len());
        out.push((attrs, content[..end].to_string()));
        rest = &content[end..];
    }
    out
}

/// `rowspan="2"` style attribute → usize (0 when absent/garbled).
fn attr_usize(attrs: &str, name: &str) -> usize {
    attrs
        .find(name)
        .and_then(|i| {
            let after = &attrs[i + name.len()..];
            let digits: String = after
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse().ok()
        })
        .unwrap_or(0)
}

/// Drop inline tags inside a cell, normalize whitespace.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Even synthetic cell bboxes over the (real) table region — the model
/// returns no geometry; same honest approximation as the VLM task. Span
/// semantics ride through from the parsed grid.
fn grid_cells(grid: &[Vec<ParsedCell>], bbox: &BBox) -> Vec<Vec<Cell>> {
    let nr = grid.len() as f32;
    let nc = grid.first().map(Vec::len).unwrap_or(0) as f32;
    let (tw, th) = (bbox.x1 - bbox.x0, bbox.y1 - bbox.y0);
    grid.iter()
        .enumerate()
        .map(|(ri, row)| {
            row.iter()
                .enumerate()
                .map(|(ci, pc)| Cell {
                    text: pc.text.clone(),
                    bbox: BBox {
                        x0: bbox.x0 + tw * ci as f32 / nc,
                        y0: bbox.y1 - th * (ri as f32 + 1.0) / nr,
                        x1: bbox.x0 + tw * (ci as f32 + 1.0) / nc,
                        y1: bbox.y1 - th * ri as f32 / nr,
                    },
                    row_span: pc.row_span,
                    col_span: pc.col_span,
                    merged: pc.merged,
                })
                .collect()
        })
        .collect()
}

/// Keep `Table` in the public signature without exporting IR internals.
pub type RefinedTable = Table;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_expand_by_replication() {
        let html = r#"<table>
<tr><td rowspan="2">A</td><td colspan="2">B</td></tr>
<tr><td>c</td><td>d</td></tr>
</table>"#;
        let g = parse_html_table(html).unwrap();
        assert_eq!(g.len(), 2);
        let texts: Vec<Vec<&str>> = g
            .iter()
            .map(|r| r.iter().map(|c| c.text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec![vec!["A", "B", "B"], vec!["A", "c", "d"]]);
        // Anchor spans + covered marks.
        assert_eq!(
            (g[0][0].row_span, g[0][0].col_span, g[0][0].merged),
            (2, 1, false)
        );
        assert_eq!((g[0][1].col_span, g[0][1].merged), (2, false));
        assert!(g[0][2].merged, "colspan-covered position");
        assert!(g[1][0].merged, "rowspan-covered position");
        assert!(!g[1][1].merged);
    }

    #[test]
    fn th_and_inline_tags_and_padding() {
        let html = "<table><tr><th>H<b>1</b></th><th>H2</th><th>H3</th></tr><tr><td>1</td><td>2</td></tr></table>";
        let g = parse_html_table(html).unwrap();
        let r0: Vec<&str> = g[0].iter().map(|c| c.text.as_str()).collect();
        let r1: Vec<&str> = g[1].iter().map(|c| c.text.as_str()).collect();
        assert_eq!(r0, vec!["H1", "H2", "H3"]);
        assert_eq!(r1, vec!["1", "2", ""]); // padded to widest
    }

    #[test]
    fn prose_and_degenerate_rejected() {
        assert!(parse_html_table("no table here").is_none());
        assert!(parse_html_table("<table><tr><td>only</td></tr></table>").is_none());
        assert!(parse_html_table(
            "<table><tr><td></td><td></td></tr><tr><td></td><td></td></tr></table>"
        )
        .is_none());
    }

    #[test]
    fn pg9_shape_header_then_data() {
        // The exact shape UniRec produced for the OTSL-paper table.
        let html = r#"<table>
<tr><td rowspan="2"># enc-layers</td><td rowspan="2"># dec-layers</td><td rowspan="2">Language</td><td colspan="3">TEDs</td><td rowspan="2">mAP (0.75)</td><td rowspan="2">Inference time (secs)</td></tr>
<tr><td>simple</td><td>complex</td><td>all</td></tr>
<tr><td rowspan="2">6</td><td rowspan="2">6</td><td>OTSL</td><td>0.965</td><td>0.934</td><td>0.955</td><td>0.88</td><td>2.73</td></tr>
<tr><td>HTML</td><td>0.969</td><td>0.927</td><td>0.955</td><td>0.857</td><td>5.39</td></tr>
</table>"#;
        let g = parse_html_table(html).unwrap();
        assert_eq!(g.len(), 4);
        assert_eq!(g[0].len(), 8);
        assert_eq!(g[1][0].text, "# enc-layers"); // rowspan replicated down
        assert!(g[1][0].merged);
        assert_eq!(g[1][3].text, "simple");
        assert_eq!(g[0][3].col_span, 3); // TEDs anchor
        assert_eq!(g[0][4].text, "TEDs"); // colspan replicated across
        assert!(g[0][4].merged);
        assert_eq!(g[3][0].text, "6"); // data rowspan
        assert_eq!(g[3][2].text, "HTML");
    }
}
