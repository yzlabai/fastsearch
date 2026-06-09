//! Layout reconstruction: positioned chunks → lines → paragraphs, plus
//! header/footer detection (roadmap module 3, "版面与阅读顺序").
//!
//! The PDF backend emits per-glyph/per-run chunks with coordinates but no
//! notion of "line" or "paragraph". This module rebuilds them geometrically:
//! group chunks sharing a baseline into [`Line`]s (inserting word spaces by
//! gap), detect running headers/footers that repeat across pages, then group
//! consecutive body lines into [`Block`]s (paragraphs / headings) by vertical
//! spacing. Output formats consume blocks instead of raw chunks so Markdown is
//! readable paragraphs, not one block per line.

use crate::ir::{BBox, Document, Page, TextChunk};
use crate::reading_order::reading_order;
use std::collections::HashMap;

/// Inter-chunk gap (in em) above which a word space is inserted. A space
/// advances ~0.25 em (veraPDF `WHITE_SPACE_FACTOR`), and veraPDF splits words at
/// 0.21 (`SPLIT_THRESHOLD_FACTOR`) — but justified body text packs word spaces
/// tighter (~0.167 em observed), while intra-word gaps stay ~0.01 em. 0.15
/// sits in that band: it catches tight word spaces without splitting words.
/// Tuned against the Docling born-digital set (NID peaks at 0.15; 0.12
/// over-splits, 0.25 under-splits). See scripts/eval/compare_docling.py.
const WORD_GAP_EM: f32 = 0.15;

/// Whether a chunk's center lies inside any of the given (table) boxes — used
/// to exclude table content from line/paragraph reconstruction.
pub fn in_any(chunk: &TextChunk, boxes: &[crate::ir::BBox]) -> bool {
    let cx = (chunk.bbox.x0 + chunk.bbox.x1) / 2.0;
    let cy = chunk.bbox.cy();
    boxes
        .iter()
        .any(|b| cx >= b.x0 && cx <= b.x1 && cy >= b.y0 && cy <= b.y1)
}

/// A reconstructed text line with the geometry later stages need.
pub struct Line {
    pub text: String,
    /// Representative (max) font size on the line.
    pub size: f32,
    /// Vertical center (baseline proxy); larger = higher on the page.
    pub cy: f32,
    pub x0: f32,
    pub x1: f32,
    pub page: usize,
    /// True when every chunk on the line is bold (heading signal).
    pub bold: bool,
}

/// A body block: a paragraph or a heading, after grouping lines. Carries page +
/// union bbox so downstream (chunking/citation) can point back to the source.
pub struct Block {
    pub text: String,
    pub size: f32,
    pub heading: bool,
    pub page: usize,
    pub bbox: BBox,
}

/// Group chunks into lines (shared baseline) and words (by gap). A horizontal
/// gap wider than ~0.25 em starts a new word. Callers pass the chunks to use
/// (e.g. with table content already excluded).
pub fn reconstruct_lines(chunks: &[&TextChunk]) -> Vec<Line> {
    let order = reading_order(chunks);

    let mut lines: Vec<Line> = Vec::new();
    // Accumulator over the current line.
    let mut cur: Option<Line> = None;

    for &i in &order {
        let c = chunks[i];
        let cy = c.bbox.cy();
        match cur.as_mut() {
            Some(line) if (line.cy - cy).abs() <= c.font_size.max(1.0) * 0.5 => {
                // Insert a word space when the inter-chunk gap exceeds the
                // word-split threshold. A real space advances ~0.25 em, so the
                // threshold must sit *below* that (a gap of exactly 0.25 em is a
                // space). Mirrors veraPDF-wcag-algs `SPLIT_THRESHOLD_FACTOR`
                // (0.21) vs `WHITE_SPACE_FACTOR` (0.25); our flat 0.25 missed
                // exactly-0.25 em spaces (e.g. "BirgitPfitzmann").
                if c.bbox.x0 - line.x1 > c.font_size * WORD_GAP_EM {
                    line.text.push(' ');
                }
                line.text.push_str(&c.text);
                line.x1 = c.bbox.x1;
                line.size = line.size.max(c.font_size);
                line.bold = line.bold && c.bold;
            }
            _ => {
                if let Some(line) = cur.take() {
                    lines.push(line);
                }
                cur = Some(Line {
                    text: c.text.clone(),
                    size: c.font_size,
                    cy,
                    x0: c.bbox.x0,
                    x1: c.bbox.x1,
                    page: c.page,
                    bold: c.bold,
                });
            }
        }
    }
    if let Some(line) = cur {
        lines.push(line);
    }
    lines
}

/// Normalize a line's text for repeat-detection (collapse whitespace, fold
/// digits to `#` so "Page 1"/"Page 2" and "1"/"2" page numbers match).
fn normalize_repeat(text: &str) -> String {
    let mut s = String::new();
    let mut prev_space = false;
    for c in text.trim().chars() {
        if c.is_whitespace() {
            if !prev_space {
                s.push(' ');
            }
            prev_space = true;
        } else {
            s.push(if c.is_ascii_digit() { '#' } else { c });
            prev_space = false;
        }
    }
    s
}

/// Texts (normalized) identified as running headers/footers — lines near the
/// top or bottom margin whose normalized text repeats across many pages.
pub struct HeaderFooter {
    repeated: std::collections::HashSet<String>,
}

impl HeaderFooter {
    /// Whether a reconstructed line should be dropped as a header/footer.
    pub fn is_running(&self, line: &Line) -> bool {
        !self.repeated.is_empty() && self.repeated.contains(&normalize_repeat(&line.text))
    }
}

/// Detect running headers/footers: among lines in the top/bottom 12% margin of
/// each page, any whose normalized text appears on at least `min(3, pages)` and
/// ≥ 50% of pages is considered running content. Single-page docs → none.
pub fn detect_header_footer(pages: &[Page], lines_per_page: &[Vec<Line>]) -> HeaderFooter {
    let n = pages.len();
    let mut empty = HeaderFooter {
        repeated: std::collections::HashSet::new(),
    };
    if n < 3 {
        return empty;
    }
    // count normalized-text -> number of distinct pages it appears in a margin
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (page, lines) in pages.iter().zip(lines_per_page) {
        let h = page.height.max(1.0);
        let top = h * 0.88;
        let bot = h * 0.12;
        let mut seen_on_page = std::collections::HashSet::new();
        for line in lines {
            if line.cy >= top || line.cy <= bot {
                let key = normalize_repeat(&line.text);
                if key.len() >= 2 {
                    seen_on_page.insert(key);
                }
            }
        }
        for key in seen_on_page {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    let threshold = 3.max(n / 2);
    for (key, c) in counts {
        if c >= threshold {
            empty.repeated.insert(key);
        }
    }
    empty
}

/// Whether a line is numeric-dominant (>40% digits among non-space chars) —
/// a cheap proxy for table/number rows we must not reflow into prose.
fn is_numeric_row(text: &str) -> bool {
    let mut digits = 0usize;
    let mut total = 0usize;
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

/// One line of an in-progress block: text + the geometry needed to decide
/// whether the next line continues it, plus the accumulating union bbox.
struct Acc {
    text: String,
    size: f32,
    cy: f32,
    /// Right edge of the most recent line (does it reach the column edge?).
    x1: f32,
    numeric: bool,
    lines: usize,
    page: usize,
    x0_min: f32,
    x1_max: f32,
    y_top: f32,
    y_bot: f32,
    bold: bool,
}

impl Acc {
    fn start(line: &Line, text: String, numeric: bool) -> Self {
        Self {
            text,
            size: line.size,
            cy: line.cy,
            x1: line.x1,
            numeric,
            lines: 1,
            page: line.page,
            x0_min: line.x0,
            x1_max: line.x1,
            y_top: line.cy + line.size / 2.0,
            y_bot: line.cy - line.size / 2.0,
            bold: line.bold,
        }
    }
    fn extend(&mut self, line: &Line, text: &str, numeric: bool) {
        self.text.push(' ');
        self.text.push_str(text);
        self.cy = line.cy;
        self.size = self.size.max(line.size);
        self.x1 = line.x1;
        self.numeric = numeric;
        self.lines += 1;
        self.bold = self.bold && line.bold;
        self.x0_min = self.x0_min.min(line.x0);
        self.x1_max = self.x1_max.max(line.x1);
        self.y_bot = self.y_bot.min(line.cy - line.size / 2.0);
    }
}

/// Group body lines (top-to-bottom) into paragraphs/headings. A line continues
/// the current paragraph only when it is clearly a wrapped prose continuation:
/// normal single-line gap, similar font size, the *previous* line reached the
/// column's right edge (`fill_x`), and neither line is a numeric/table row.
/// Otherwise it starts a new block; a lone larger-than-median line is a heading.
/// This conservatism keeps tables/lists (short or numeric lines) one-per-line
/// instead of mashing them into a blob.
///
/// TODO: left columns in multi-column pages don't reach the page-wide `fill_x`,
/// so their prose isn't reflowed yet — needs per-column edges (M4).
pub fn group_blocks(lines: &[Line], body_size: f32, fill_x: f32) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur: Option<Acc> = None;

    for line in lines {
        let t = line.text.trim();
        if t.is_empty() {
            continue;
        }
        let numeric = is_numeric_row(t);
        let continues = cur.as_ref().is_some_and(|a| {
            (a.cy - line.cy) <= a.size.max(1.0) * 1.8
                && (line.size - a.size).abs() <= a.size * 0.2
                && a.x1 >= fill_x
                && !a.numeric
                && !numeric
        });

        match cur.as_mut() {
            Some(a) if continues => a.extend(line, t, numeric),
            _ => {
                if let Some(a) = cur.take() {
                    blocks.push(make_block(a, body_size));
                }
                cur = Some(Acc::start(line, t.to_string(), numeric));
            }
        }
    }
    if let Some(a) = cur {
        blocks.push(make_block(a, body_size));
    }
    blocks
}

/// Recognize a heading by text shape (single short line): a numbered section
/// ("1 Introduction", "5.1 Hyper Parameter…") or an all-caps title
/// ("ABSTRACT", "REFERENCES"). Catches the bold/body-size section headers that
/// the font-size rule misses in 2-column papers. Conservative length gate
/// avoids flagging sentences/list items.
fn is_heading_text(t: &str) -> bool {
    let t = t.trim();
    let nchars = t.chars().count();
    if !(2..=55).contains(&nchars) {
        return false;
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    let numbered = words.len() >= 2
        && {
            let w = words[0];
            w.chars().all(|c| c.is_ascii_digit() || c == '.') && w.chars().any(|c| c.is_ascii_digit())
        }
        && words[1].chars().next().is_some_and(|c| c.is_uppercase());
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    let all_caps = letters.len() >= 2 && letters.iter().all(|c| c.is_uppercase());
    numbered || all_caps
}

fn make_block(a: Acc, body_size: f32) -> Block {
    // A heading is a single-line block that is notably larger than body text,
    // whose text shape (numbered / all-caps) reads like a section header, or a
    // short fully-bold line (title-case subsection at body size).
    let short = a.text.chars().count() <= 60;
    let heading = a.lines == 1
        && ((body_size > 0.0 && a.size > body_size * 1.25)
            || is_heading_text(&a.text)
            || (a.bold && short));
    Block {
        text: a.text,
        size: a.size,
        heading,
        page: a.page,
        bbox: BBox { x0: a.x0_min, y0: a.y_bot, x1: a.x1_max, y1: a.y_top },
    }
}

/// Full reconstruction pipeline per page: exclude text inside detected tables,
/// drop running headers/footers, group into paragraph/heading [`Block`]s.
/// Shared by output serialization and RAG chunking so they agree on structure.
pub fn page_blocks(doc: &Document) -> Vec<Vec<Block>> {
    use crate::ir::Element;
    let table_boxes: Vec<Vec<BBox>> = doc
        .pages
        .iter()
        .map(|p| {
            p.elements
                .iter()
                .filter_map(|e| match e {
                    Element::Table(t) => Some(t.bbox),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let chunks_per_page: Vec<Vec<&TextChunk>> = doc
        .pages
        .iter()
        .zip(&table_boxes)
        .map(|(p, boxes)| p.text_chunks().into_iter().filter(|c| !in_any(c, boxes)).collect())
        .collect();
    let lines_per_page: Vec<Vec<Line>> =
        chunks_per_page.iter().map(|cs| reconstruct_lines(cs)).collect();
    let hf = detect_header_footer(&doc.pages, &lines_per_page);
    let body = body_font_size(doc);

    lines_per_page
        .into_iter()
        .zip(&doc.pages)
        .map(|(lines, page)| {
            let body_lines: Vec<Line> = lines.into_iter().filter(|l| !hf.is_running(l)).collect();
            let right = body_lines.iter().map(|l| l.x1).fold(0.0f32, f32::max);
            let fill_x = right - page.width.max(1.0) * 0.05;
            group_blocks(&body_lines, body, fill_x)
        })
        .collect()
}

/// Body font size: the most common chunk size (mode, in 0.5 pt bins). More
/// robust than the median for heading detection — a doc with many headings
/// inflates the median, but body text is still the *most frequent* size.
/// Deterministic tie-break: highest count, then smallest size.
pub fn body_font_size(doc: &Document) -> f32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for c in doc.pages.iter().flat_map(|p| p.text_chunks()) {
        *counts.entry((c.font_size * 2.0).round() as u32).or_insert(0) += 1;
    }
    let mut entries: Vec<(u32, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    entries.first().map(|&(k, _)| k as f32 / 2.0).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BBox, Element, Page, TextChunk};

    fn line(text: &str, size: f32, cy: f32) -> Line {
        line_w(text, size, cy, 100.0)
    }
    fn line_w(text: &str, size: f32, cy: f32, x1: f32) -> Line {
        Line { text: text.into(), size, cy, x0: 0.0, x1, page: 1, bold: false }
    }

    // fill_x = 90: lines reaching x1≈100 count as wrapped prose.
    const FILL: f32 = 90.0;

    #[test]
    fn paragraph_merges_close_lines_breaks_on_gap() {
        let lines = vec![
            line("First line of para", 10.0, 200.0),
            line("second line continues", 10.0, 188.0), // gap 12 < 18, fills → merge
            line("A new paragraph", 10.0, 150.0),        // gap 38 > 18 → break
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "First line of para second line continues");
        assert_eq!(blocks[1].text, "A new paragraph");
    }

    #[test]
    fn numbered_and_allcaps_lines_are_headings() {
        // Body-size lines that read like section headers (gaps > 18 → separate).
        let lines = vec![
            line("1 Introduction", 10.0, 200.0),
            line("ABSTRACT", 10.0, 178.0),
            line("ordinary body sentence continues here", 10.0, 156.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert!(blocks[0].heading, "numbered section header");
        assert!(blocks[1].heading, "all-caps header");
        assert!(!blocks[2].heading, "body text is not a heading");
    }

    #[test]
    fn larger_lone_line_is_heading() {
        let lines = vec![
            line("Big Title", 20.0, 300.0),
            line("body text here", 10.0, 270.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert!(blocks[0].heading);
        assert!(!blocks[1].heading);
    }

    #[test]
    fn short_lines_do_not_merge_into_a_blob() {
        // Evenly-spaced short lines (table/list labels) must stay one-per-line,
        // because none reach the column edge (x1=40 < fill_x=90).
        let lines = vec![
            line_w("Ricavi delle vendite", 10.0, 200.0, 40.0),
            line_w("Variazione rimanenze", 10.0, 188.0, 40.0),
            line_w("Altri ricavi e proventi", 10.0, 176.0, 40.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn numeric_rows_do_not_merge() {
        // Full-width numeric rows (financial table) must not reflow together.
        let lines = vec![
            line("124.504.000 120.062.000 1.942.000", 10.0, 200.0),
            line("127.608.000 124.406.000 117.000", 10.0, 188.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 2);
    }

    fn page_with_lines(number: usize, texts: &[(&str, f32)], height: f32) -> Page {
        let elements = texts
            .iter()
            .map(|(t, cy)| {
                Element::Text(TextChunk {
                    text: t.to_string(),
                    bbox: BBox { x0: 0.0, y0: cy - 5.0, x1: 50.0, y1: cy + 5.0 },
                    font_size: 10.0,
                    font: None,
                    page: number,
                    confidence: 1.0,
            bold: false,
                })
            })
            .collect();
        Page { number, width: 200.0, height, elements }
    }

    #[test]
    fn detects_repeated_footer_page_numbers() {
        // 4 pages, each with a top title (unique) and a bottom "Page N" footer.
        let pages: Vec<Page> = (1..=4)
            .map(|i| {
                page_with_lines(
                    i,
                    &[("Unique body of page", 400.0), (&format!("Page {i}"), 10.0)],
                    800.0,
                )
            })
            .collect();
        let lpp: Vec<Vec<Line>> = pages.iter().map(|p| reconstruct_lines(&p.text_chunks())).collect();
        let hf = detect_header_footer(&pages, &lpp);
        // "Page #" (digits folded) should be flagged; body should not.
        assert!(hf.is_running(&line("Page 1", 10.0, 10.0)));
        assert!(!hf.is_running(&line("Unique body of page", 10.0, 400.0)));
    }

    #[test]
    fn single_page_has_no_running_content() {
        let pages = vec![page_with_lines(1, &[("Footer", 10.0)], 800.0)];
        let lpp: Vec<Vec<Line>> = pages.iter().map(|p| reconstruct_lines(&p.text_chunks())).collect();
        let hf = detect_header_footer(&pages, &lpp);
        assert!(!hf.is_running(&line("Footer", 10.0, 10.0)));
    }
}
