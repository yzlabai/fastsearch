//! DOCX backend: read Office Open XML with `docx-rs` and flow its paragraphs,
//! headings and tables onto a synthetic page (see `docparse_core::synth`).
//!
//! DOCX has explicit structure (paragraph styles, table grids) but no
//! coordinates, so geometry is fabricated under the PDF convention and the
//! shared reading-order/output layers take over. Heading levels come from the
//! paragraph style name ("Heading1" …); tables map straight to `Table`.

use docparse_core::ir::{Document, Page, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::{PageBuilder, SpanCell};
use docx_rs::{
    DocumentChild, Docx, Level, Numberings, Paragraph, ParagraphChild, RunChild, Table, TableCell,
    TableCellContent, TableChild, TableRowChild,
};
use std::collections::HashMap;
use std::path::Path;

pub struct DocxParser;

impl DocumentParser for DocxParser {
    fn name(&self) -> &'static str {
        "docx"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("docx"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let buf = std::fs::read(path)?;
        let mut doc = parse_bytes(&buf)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// Parse DOCX bytes into a [`Document`].
pub fn parse_bytes(buf: &[u8]) -> anyhow::Result<Document> {
    // Resource guard (N5b): refuse zip-bomb-shaped archives by reading only the
    // central directory — before docx-rs decompresses anything.
    docparse_core::limits::check_zip_bomb(buf)?;
    let docx = docx_rs::read_docx(buf).map_err(|e| anyhow::anyhow!("docx parse: {e}"))?;
    Ok(Document {
        source: "<docx>".to_string(),
        provenance: Some(Provenance::new("docx", env!("CARGO_PKG_VERSION"))),
        pages: document_pages(&docx),
    })
}

/// Flow a parsed DOCX onto synthetic pages: paragraphs (headings sized by style,
/// `w:numPr` list items marked + `LI`-tagged) and tables. Split out so the
/// mapping can be unit-tested on a builder-constructed `Docx` without a byte
/// round-trip.
fn document_pages(docx: &Docx) -> Vec<Page> {
    let mut b = PageBuilder::letter();
    // Ordered-list counters keyed by (numId, ilvl); entering a level resets its
    // deeper levels (see `list_marker`).
    let mut counters: HashMap<(usize, usize), u64> = HashMap::new();
    for child in &docx.document.children {
        match child {
            DocumentChild::Paragraph(p) => {
                let size = paragraph_size(p);
                // List items are body paragraphs; a numbered heading stays a
                // heading (its larger size keeps it out of the list path).
                let marker = if size <= 12.0 {
                    list_marker(p, &docx.numberings, &mut counters)
                } else {
                    None
                };
                match marker {
                    Some(m) => b.list_item(format!("{m}{}", paragraph_text(p)), size),
                    None => b.paragraph(paragraph_text(p), size),
                }
            }
            DocumentChild::Table(t) => {
                b.table_spanned(table_rows_spanned(t), 12.0);
            }
            _ => {}
        }
    }
    b.finish()
}

/// Resolve a paragraph's list marker, or `None` if it is not a list item.
/// `w:numPr` (numId + ilvl) is resolved through the numbering definitions to a
/// level format: `bullet` → `• `, `none` → no marker, anything else (decimal /
/// lowerLetter / …) → an ordered `N. ` whose counter advances per item and
/// honors the level's start. Entering level `ilvl` restarts every deeper level
/// of the same list (nested-list restart). A list item whose definition is
/// missing still gets a bullet — `numPr` already proves it is a list item.
fn list_marker(
    p: &Paragraph,
    numberings: &Numberings,
    counters: &mut HashMap<(usize, usize), u64>,
) -> Option<String> {
    let np = p.property.numbering_property.as_ref()?;
    let num_id = np.id.as_ref()?.id;
    let ilvl = np.level.as_ref().map(|l| l.val).unwrap_or(0);
    // Entering this level restarts every deeper level of the same list.
    counters.retain(|&(nid, l), _| !(nid == num_id && l > ilvl));

    let level = resolve_level(numberings, num_id, ilvl);
    let fmt = level.map(|l| l.format.val.as_str()).unwrap_or("bullet");
    match fmt {
        "bullet" => Some("• ".to_string()),
        "none" => Some(String::new()),
        _ => {
            use std::collections::hash_map::Entry;
            let count = match counters.entry((num_id, ilvl)) {
                Entry::Occupied(mut e) => {
                    *e.get_mut() += 1;
                    *e.get()
                }
                // The start ordinal is only needed for a level's first item;
                // read it (a serde round-trip) lazily, not on every item.
                Entry::Vacant(e) => *e.insert(level.and_then(level_start).unwrap_or(1)),
            };
            Some(format!("{count}. "))
        }
    }
}

/// Resolve `(numId, ilvl)` to its level definition: numId → abstractNumId →
/// the matching level.
fn resolve_level(n: &Numberings, num_id: usize, ilvl: usize) -> Option<&Level> {
    let abs_id = n
        .numberings
        .iter()
        .find(|x| x.id == num_id)?
        .abstract_num_id;
    let abs = n.abstract_nums.iter().find(|a| a.id == abs_id)?;
    abs.levels.iter().find(|l| l.level == ilvl)
}

/// The level's starting ordinal. `Start` has no getter but serializes as a bare
/// integer (docx-rs's public `Serialize` contract); defaults to 1.
fn level_start(level: &Level) -> Option<u64> {
    serde_json::to_value(&level.start).ok()?.as_u64()
}

/// Concatenate a paragraph's run text.
fn paragraph_text(p: &Paragraph) -> String {
    let mut s = String::new();
    for child in &p.children {
        if let ParagraphChild::Run(run) = child {
            for rc in &run.children {
                match rc {
                    RunChild::Text(t) => s.push_str(&t.text),
                    RunChild::Tab(_) => s.push('\t'),
                    _ => {}
                }
            }
        }
    }
    s
}

/// Font size from the paragraph style name ("Heading1" …, "Title").
fn paragraph_size(p: &Paragraph) -> f32 {
    let style = p
        .property
        .style
        .as_ref()
        .map(|s| s.val.as_str())
        .unwrap_or("");
    let lower = style.to_ascii_lowercase();
    if lower == "title" {
        return 26.0;
    }
    match lower
        .strip_prefix("heading")
        .and_then(|n| n.trim().parse::<u32>().ok())
    {
        Some(1) => 24.0,
        Some(2) => 20.0,
        Some(3) => 17.0,
        Some(4) => 15.0,
        Some(_) => 13.0,
        None => 12.0,
    }
}

/// Concatenate a DOCX table cell's paragraph text (space-joined).
fn cell_text(cell: &TableCell) -> String {
    let mut text = String::new();
    for content in &cell.children {
        if let TableCellContent::Paragraph(p) = content {
            let t = paragraph_text(p);
            if !t.is_empty() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&t);
            }
        }
    }
    text
}

/// A cell's vertical-merge state (`w:vMerge`).
enum VMergeKind {
    Restart,  // top of a vertical span
    Continue, // covered by the span above
}

/// Read a cell's horizontal span (`w:gridSpan`) and vertical-merge state through
/// docx-rs's public `Serialize` form — the property fields have no getters. The
/// JSON shape (`gridSpan` int, `verticalMerge` "restart"/"continue") is pinned by
/// docx-rs's own tests; anything missing or unexpected degrades to a plain cell.
fn cell_span(cell: &TableCell) -> (u32, Option<VMergeKind>) {
    let v = serde_json::to_value(&cell.property).unwrap_or(serde_json::Value::Null);
    let col_span = v
        .get("gridSpan")
        .and_then(|x| x.as_u64())
        .unwrap_or(1)
        .max(1) as u32;
    let vmerge = match v.get("verticalMerge").and_then(|x| x.as_str()) {
        Some("restart") => Some(VMergeKind::Restart),
        Some("continue") => Some(VMergeKind::Continue),
        _ => None,
    };
    (col_span, vmerge)
}

/// Build a sparse span grid from a DOCX table. `gridSpan` becomes `col_span`;
/// `vMerge` is normalized to a `row_span` on the merge's anchor (the `restart`
/// cell): each `continue` cell is dropped and bumps the anchor's row_span, so the
/// grid expander materializes the covered positions exactly like an HTML rowspan.
fn table_rows_spanned(t: &Table) -> Vec<Vec<SpanCell>> {
    let mut sparse: Vec<Vec<SpanCell>> = Vec::new();
    // Open vertical merges, keyed by grid column → (sparse row, index in that row).
    let mut open: HashMap<usize, (usize, usize)> = HashMap::new();
    for TableChild::TableRow(row) in &t.rows {
        let mut sparse_row: Vec<SpanCell> = Vec::new();
        let mut gc = 0usize; // running grid column (counts spanned columns)
        for TableRowChild::TableCell(cell) in &row.cells {
            let (col_span, vmerge) = cell_span(cell);
            // A `continue` cell covered by an open merge: bump the anchor's
            // row_span and emit nothing — the expander fills the covered slot.
            if matches!(vmerge, Some(VMergeKind::Continue)) {
                if let Some(&(ar, ai)) = open.get(&gc) {
                    sparse[ar][ai].row_span += 1;
                    gc += col_span as usize;
                    continue;
                }
                // Orphan `continue` (no restart above): fall through as own cell.
            }
            let idx = sparse_row.len();
            sparse_row.push(SpanCell {
                text: cell_text(cell),
                row_span: 1,
                col_span,
            });
            match vmerge {
                Some(VMergeKind::Restart) => {
                    open.insert(gc, (sparse.len(), idx));
                }
                // A plain cell (or orphan continue) closes any merge in this column.
                _ => {
                    open.remove(&gc);
                }
            }
            gc += col_span as usize;
        }
        sparse.push(sparse_row);
    }
    sparse
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::Element;
    use docparse_core::synth::PageBuilder;
    use docx_rs::{
        AbstractNumbering, IndentLevel, LevelJc, LevelText, NumberFormat, Numbering, NumberingId,
        Run, Start, TableRow, VMergeType,
    };

    fn cell(text: &str) -> TableCell {
        TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text(text)))
    }

    /// A numbering level with the given index and numFmt (start = 1).
    fn level(n: usize, fmt: &str) -> Level {
        Level::new(
            n,
            Start::new(1),
            NumberFormat::new(fmt),
            LevelText::new("%1."),
            LevelJc::new("left"),
        )
    }

    /// A body paragraph attached to numbering id 1 at the given level.
    fn list_para(lvl: usize, text: &str) -> Paragraph {
        Paragraph::new()
            .add_run(Run::new().add_text(text))
            .numbering(NumberingId::new(1), IndentLevel::new(lvl))
    }

    // TC-05: gridSpan → col_span on the anchor (covered columns are filled by
    // expand_spans downstream, not in the sparse grid).
    #[test]
    fn gridspan_becomes_colspan() {
        let t = Table::new(vec![
            TableRow::new(vec![cell("H").grid_span(2)]),
            TableRow::new(vec![cell("a"), cell("b")]),
        ]);
        let sparse = table_rows_spanned(&t);
        assert_eq!(
            sparse[0].len(),
            1,
            "one anchor cell, covered column omitted"
        );
        assert_eq!(
            (sparse[0][0].text.as_str(), sparse[0][0].col_span),
            ("H", 2)
        );
        assert_eq!(sparse[1].len(), 2);
    }

    // TC-06: vMerge restart/continue → row_span on the anchor; the continue cell
    // is dropped (the expander then materializes the covered position).
    #[test]
    fn vmerge_becomes_rowspan_on_anchor() {
        let t = Table::new(vec![
            TableRow::new(vec![
                cell("A").vertical_merge(VMergeType::Restart),
                cell("b1"),
            ]),
            TableRow::new(vec![
                cell("").vertical_merge(VMergeType::Continue),
                cell("c2"),
            ]),
        ]);
        let sparse = table_rows_spanned(&t);
        assert_eq!(
            (sparse[0][0].text.as_str(), sparse[0][0].row_span),
            ("A", 2)
        );
        assert_eq!(sparse[1].len(), 1, "continue cell dropped");
        assert_eq!(sparse[1][0].text, "c2");
    }

    // TC-07: an orphan `continue` (no `restart` above) must not panic or vanish.
    #[test]
    fn orphan_continue_is_kept_as_own_cell() {
        let t = Table::new(vec![TableRow::new(vec![
            cell("x").vertical_merge(VMergeType::Continue),
            cell("y"),
        ])]);
        let sparse = table_rows_spanned(&t);
        assert_eq!(sparse[0].len(), 2);
        assert_eq!(sparse[0][0].text, "x");
    }

    // End to end through the synth grid: the covered position materializes with
    // the replicated anchor text and `merged = true`.
    #[test]
    fn vmerge_materializes_covered_cell_via_pagebuilder() {
        let t = Table::new(vec![
            TableRow::new(vec![
                cell("A").vertical_merge(VMergeType::Restart),
                cell("b1"),
            ]),
            TableRow::new(vec![
                cell("").vertical_merge(VMergeType::Continue),
                cell("c2"),
            ]),
        ]);
        let mut b = PageBuilder::letter();
        b.table_spanned(table_rows_spanned(&t), 12.0);
        let pages = b.finish();
        let table = pages
            .iter()
            .flat_map(|p| &p.elements)
            .find_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            (table.rows[0][0].row_span, table.rows[0][0].merged),
            (2, false)
        );
        assert!(table.rows[1][0].merged);
        assert_eq!(table.rows[1][0].text, "A", "covered text replicated");
        assert_eq!(table.rows[1][1].text, "c2");
    }

    // TC-01: bullet vs ordered marker; ordered counter advances; plain → None.
    #[test]
    fn list_markers_bullet_ordered_and_counters() {
        let numberings = Numberings::new()
            .add_abstract_numbering(
                AbstractNumbering::new(0)
                    .add_level(level(0, "decimal"))
                    .add_level(level(1, "bullet")),
            )
            .add_numbering(Numbering::new(1, 0));
        let mut counters = HashMap::new();
        assert_eq!(
            list_marker(&list_para(0, "a"), &numberings, &mut counters),
            Some("1. ".into())
        );
        assert_eq!(
            list_marker(&list_para(0, "b"), &numberings, &mut counters),
            Some("2. ".into())
        );
        assert_eq!(
            list_marker(&list_para(1, "x"), &numberings, &mut counters),
            Some("• ".into())
        );
        assert_eq!(
            list_marker(&list_para(0, "c"), &numberings, &mut counters),
            Some("3. ".into())
        );
        let plain = Paragraph::new().add_run(Run::new().add_text("p"));
        assert_eq!(list_marker(&plain, &numberings, &mut counters), None);
    }

    // TC-02: a deeper level restarts each time its parent level advances.
    #[test]
    fn nested_ordered_levels_restart_on_reentry() {
        let numberings = Numberings::new()
            .add_abstract_numbering(
                AbstractNumbering::new(0)
                    .add_level(level(0, "decimal"))
                    .add_level(level(1, "decimal")),
            )
            .add_numbering(Numbering::new(1, 0));
        let mut counters = HashMap::new();
        let m = |lvl, c: &mut HashMap<(usize, usize), u64>| {
            list_marker(&list_para(lvl, "x"), &numberings, c)
        };
        assert_eq!(m(0, &mut counters), Some("1. ".into()));
        assert_eq!(m(1, &mut counters), Some("1. ".into()));
        assert_eq!(m(1, &mut counters), Some("2. ".into()));
        assert_eq!(m(0, &mut counters), Some("2. ".into())); // advances level 0, resets level 1
        assert_eq!(m(1, &mut counters), Some("1. ".into())); // restarted
    }

    // TC-03: end to end — list paragraphs become LI-tagged marked items; a
    // non-list paragraph stays a plain paragraph.
    #[test]
    fn list_paragraphs_become_marked_list_items() {
        let docx = Docx::new()
            .add_abstract_numbering(AbstractNumbering::new(0).add_level(level(0, "decimal")))
            .add_numbering(Numbering::new(1, 0))
            .add_paragraph(list_para(0, "first"))
            .add_paragraph(list_para(0, "second"))
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("body")));
        let pages = document_pages(&docx);
        let items: Vec<(String, Option<String>)> = pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.tag.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(items[0], ("1. first".to_string(), Some("LI".to_string())));
        assert_eq!(items[1], ("2. second".to_string(), Some("LI".to_string())));
        assert_eq!(items[2], ("body".to_string(), None));
    }
}
