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

/// A vertical/rotated text run (taller than wide, multi-char) — marginalia like
/// a sideways stamp, not part of the body flow.
fn is_vertical(c: &TextChunk) -> bool {
    c.text.chars().count() >= 4 && c.bbox.height() > c.bbox.width() * 2.0
}

/// Whether a font's PostScript name reads as monospace. Catches the common
/// families (Courier/…Mono/Menlo/Consolas/Monaco) plus TeX typewriter (cmtt)
/// and "Typewriter" faces. TODO: the FontDescriptor FixedPitch flag would be
/// authoritative; name-based covers the fonts seen in practice.
/// "H" → level 1, "H1".."H6" → that level; `None` for non-heading roles.
fn heading_tag_level(tag: Option<&str>) -> Option<u8> {
    let t = tag?;
    if !t.starts_with('H') || t.len() > 2 {
        return None;
    }
    match t[1..].parse::<u8>() {
        Ok(n) if (1..=6).contains(&n) => Some(n),
        _ if t == "H" => Some(1),
        _ => None,
    }
}

/// Roles that are author-declared NOT-headings (paragraphs, figures,
/// captions, table/list content). Containers like Span/Div carry no signal.
fn is_nonheading_tag(tag: Option<&str>) -> bool {
    matches!(
        tag,
        Some(
            "P" | "Figure"
                | "Caption"
                | "Table"
                | "TR"
                | "TD"
                | "TH"
                | "L"
                | "LI"
                | "LBody"
                | "Lbl"
                | "TOC"
                | "TOCI"
                | "Note"
                | "Code"
                | "Formula"
        )
    )
}

fn is_mono_font(name: Option<&str>) -> bool {
    let Some(n) = name else { return false };
    let l = n.to_ascii_lowercase();
    l.contains("mono")
        || l.contains("courier")
        || l.contains("menlo")
        || l.contains("consolas")
        || l.contains("monaco")
        || l.contains("typewriter")
        || l.contains("cmtt")
}

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
    /// True when every chunk on the line uses a monospace font (code signal).
    pub mono: bool,
    /// True when the line came from a Form XObject (figure/stamp content —
    /// excluded from heading classification).
    pub form: bool,
    /// H1..H6 structure-tag level (tagged PDF) — author-declared heading,
    /// overrides the geometric heuristics.
    pub tag_level: Option<u8>,
    /// True when the line carries an author-declared NOT-heading role
    /// (P/Figure/Caption/…) — vetoes the geometric heading heuristics.
    pub tagged_body: bool,
}

/// A body block: a paragraph or a heading, after grouping lines. Carries page +
/// union bbox so downstream (chunking/citation) can point back to the source.
pub struct Block {
    pub text: String,
    pub size: f32,
    pub heading: bool,
    /// Heading level (1 = top). 0 on body/code blocks. Tagged PDFs supply it
    /// directly; otherwise document-wide font-size tiers assign it (G9c).
    pub level: u8,
    /// A monospace code block (≥2 mono lines); `text` preserves line breaks
    /// and geometric indentation. Renders fenced in Markdown.
    pub code: bool,
    pub page: usize,
    pub bbox: BBox,
}

/// Group chunks into lines (shared baseline) and words (by gap). A horizontal
/// gap wider than ~0.25 em starts a new word. Callers pass the chunks to use
/// (e.g. with table content already excluded).
///
/// Reading groups (G2 layout enhancer): when chunks carry `group`, a layout
/// model has dictated the macro order — each group is reconstructed
/// separately, in group order, with the usual XY-cut geometry *inside* it.
/// Ungrouped chunks (if mixed in) sort after all groups.
pub fn reconstruct_lines(chunks: &[&TextChunk]) -> Vec<Line> {
    if chunks.iter().any(|c| c.group.is_some()) {
        let mut groups: std::collections::BTreeMap<u32, Vec<&TextChunk>> =
            std::collections::BTreeMap::new();
        for c in chunks {
            groups
                .entry(c.group.unwrap_or(u32::MAX))
                .or_default()
                .push(c);
        }
        return groups
            .into_values()
            .flat_map(|g| reconstruct_lines_inner(&g))
            .collect();
    }
    reconstruct_lines_inner(chunks)
}

fn reconstruct_lines_inner(chunks: &[&TextChunk]) -> Vec<Line> {
    // Drop vertical/rotated marginalia (e.g. the sideways arXiv stamp): a
    // multi-char chunk whose box is much taller than wide. It otherwise
    // pollutes reading order and gets misread as a heading.
    let chunks: Vec<&TextChunk> = chunks.iter().copied().filter(|c| !is_vertical(c)).collect();
    let chunks = chunks.as_slice();
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
                line.mono = line.mono && is_mono_font(c.font.as_deref());
                line.form = line.form && c.source.as_deref() == Some("form");
                line.tag_level = line.tag_level.or(heading_tag_level(c.tag.as_deref()));
                line.tagged_body = line.tagged_body || is_nonheading_tag(c.tag.as_deref());
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
                    mono: is_mono_font(c.font.as_deref()),
                    form: c.source.as_deref() == Some("form"),
                    tag_level: heading_tag_level(c.tag.as_deref()),
                    tagged_body: is_nonheading_tag(c.tag.as_deref()),
                });
            }
        }
    }
    if let Some(line) = cur {
        lines.push(line);
    }
    lines
}

/// Normalize a line's text for repeat-detection (collapse whitespace, fold each
/// run of digits to a single `#` so page numbers of different widths match —
/// "2 M. Lysak" and "10 M. Lysak" both become "# m. lysak", and "Page 1"/"Page
/// 2" match). Without run-collapsing, a multi-digit page (## ) wouldn't match a
/// single-digit one (#), and the running header would slip through as a heading.
fn normalize_repeat(text: &str) -> String {
    let mut s = String::new();
    let mut prev_space = false;
    let mut prev_hash = false;
    for c in text.trim().chars() {
        if c.is_whitespace() {
            if !prev_space {
                s.push(' ');
            }
            prev_space = true;
            prev_hash = false;
        } else if c.is_ascii_digit() {
            if !prev_hash {
                s.push('#');
            }
            prev_hash = true;
            prev_space = false;
        } else {
            s.push(c);
            prev_hash = false;
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
            // Test the line's edges, not its center: a running header sits at
            // the very top but its center can fall a hair below the 12% band
            // (e.g. 2305 header center 696.6 vs cutoff 697). Counting a line
            // whose top edge reaches the top band (or bottom edge the bottom
            // band) catches it; the cross-page repeat threshold still gates FPs.
            let top_edge = line.cy + line.size / 2.0;
            let bot_edge = line.cy - line.size / 2.0;
            if top_edge >= top || bot_edge <= bot {
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
    mono: bool,
    form: bool,
    tag_level: Option<u8>,
    tagged_body: bool,
    /// Per-line (x0, text) — kept for code blocks, whose reassembly needs
    /// line breaks and geometric indentation instead of paragraph joining.
    raw: Vec<(f32, String)>,
}

impl Acc {
    fn start(line: &Line, text: String, numeric: bool) -> Self {
        let text2 = text.clone();
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
            mono: line.mono,
            form: line.form,
            tag_level: line.tag_level,
            tagged_body: line.tagged_body,
            raw: vec![(line.x0, text2)],
        }
    }
    fn extend(&mut self, line: &Line, text: &str, numeric: bool) {
        // De-hyphenate a soft line-break hyphen: "com-" + "pact" → "compact"
        // (letter before the trailing hyphen, lowercase start next). Standard
        // in text extractors; matches Docling's rejoining.
        let soft_hyphen = self.text.ends_with('-')
            && self
                .text
                .chars()
                .rev()
                .nth(1)
                .is_some_and(|c| c.is_alphabetic())
            && text.chars().next().is_some_and(|c| c.is_lowercase());
        if soft_hyphen {
            self.text.pop();
            self.text.push_str(text);
        } else {
            self.text.push(' ');
            self.text.push_str(text);
        }
        self.cy = line.cy;
        self.size = self.size.max(line.size);
        self.x1 = line.x1;
        self.numeric = numeric;
        self.lines += 1;
        self.bold = self.bold && line.bold;
        self.mono = self.mono && line.mono;
        self.form = self.form && line.form;
        self.tag_level = self.tag_level.or(line.tag_level);
        self.tagged_body = self.tagged_body || line.tagged_body;
        self.raw.push((line.x0, text.to_string()));
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
            let gap_ok = (a.cy - line.cy) <= a.size.max(1.0) * 1.8;
            let size_ok = (line.size - a.size).abs() <= a.size * 0.2;
            // Monospace runs chain into code blocks regardless of the prose
            // gates: code lines are short (never reach fill_x) and often
            // numeric — the font itself is the continuation signal (G8a).
            if a.mono && line.mono {
                return gap_ok && size_ok;
            }
            gap_ok && size_ok && a.x1 >= fill_x && !a.numeric && !numeric
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
    demote_heading_runs(&mut blocks);
    blocks
}

/// Demote long runs of consecutive headings to body. A real document never
/// stacks many section headers back-to-back with no body between — but a code
/// block (each line `RETURN`/`CASE`/`END` tripping the all-caps/size rule) or an
/// over-segmented region does, flooding the heading set (redp5110: 100 headings
/// vs 22). Keep the first of a long run as a plausible header; demote the rest.
fn demote_heading_runs(blocks: &mut [Block]) {
    const MAX_RUN: usize = 3;
    let mut i = 0;
    while i < blocks.len() {
        if !blocks[i].heading {
            i += 1;
            continue;
        }
        let start = i;
        while i < blocks.len() && blocks[i].heading {
            i += 1;
        }
        if i - start >= MAX_RUN {
            for b in &mut blocks[start + 1..i] {
                // Author-declared (tagged) headings are never demoted.
                if b.level == 0 {
                    b.heading = false;
                }
            }
        }
    }
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
            w.chars().all(|c| c.is_ascii_digit() || c == '.')
                && w.chars().any(|c| c.is_ascii_digit())
        }
        && words[1].chars().next().is_some_and(|c| c.is_uppercase());
    let letters: Vec<char> = t.chars().filter(|c| c.is_alphabetic()).collect();
    let all_caps = letters.len() >= 2 && letters.iter().all(|c| c.is_uppercase());
    numbered || all_caps
}

/// A line that reads like code or tabular data, not a section header: it carries
/// operator punctuation that real titles never do (`= ; { } < >`) or trails a
/// comma/semicolon. Catches SQL/data lines (`USER = ALICE`, `ENABLE ;`,
/// `SET OPTION USRPRF=*OWNER`) that otherwise slip through the size/all-caps
/// heading rules and pollute the heading set. Deliberately NOT flagging
/// parentheses, dots, quotes, or underscores — real headings carry those
/// (`The Modern Era (1990s - Present)`, `VERIFY_GROUP_FOR_USER function`).
fn looks_like_code(t: &str) -> bool {
    let s = t.trim();
    s.contains(['=', ';', '{', '}', '<', '>']) || s.ends_with([',', ';'])
}

fn make_block(a: Acc, body_size: f32) -> Block {
    // Monospace runs of 2+ lines are code blocks: keep line breaks and
    // reconstruct indentation from geometry (leading spaces are positioned,
    // not encoded — one indent step ≈ 0.5 em per char cell). (G8a)
    if a.mono && a.lines >= 2 {
        let min_x0 = a.raw.iter().map(|(x, _)| *x).fold(f32::INFINITY, f32::min);
        let cell = (a.size * 0.5).max(1.0);
        let text = a
            .raw
            .iter()
            .map(|(x0, t)| {
                let indent = (((x0 - min_x0) / cell).round() as usize).min(40);
                format!("{}{}", " ".repeat(indent), t)
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Block {
            text,
            size: a.size,
            heading: false,
            level: 0,
            code: true,
            page: a.page,
            bbox: BBox {
                x0: a.x0_min,
                y0: a.y_bot,
                x1: a.x1_max,
                y1: a.y_top,
            },
        };
    }
    // A heading is a single-line block that is notably larger than body text,
    // whose text shape (numbered / all-caps) reads like a section header, or a
    // short fully-bold line (title-case subsection at body size) — and never a
    // code/data line.
    let short = a.text.chars().count() <= 60;
    // Author-declared semantics (tagged PDFs) override the geometric rules:
    // H1..H6 forces a heading, P/Figure/Caption/… vetoes one.
    let heading = a.tag_level.is_some()
        || (!a.tagged_body
            && a.lines == 1
            && !looks_like_code(&a.text)
            && !a.mono
            && !a.form
            && ((body_size > 0.0 && a.size > body_size * 1.25)
                || is_heading_text(&a.text)
                || (a.bold && short)));
    Block {
        text: a.text,
        size: a.size,
        heading,
        // Tagged level now; geometric tiers are assigned document-wide later.
        level: if heading { a.tag_level.unwrap_or(0) } else { 0 },
        code: false,
        page: a.page,
        bbox: BBox {
            x0: a.x0_min,
            y0: a.y_bot,
            x1: a.x1_max,
            y1: a.y_top,
        },
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
        .map(|(p, boxes)| {
            p.text_chunks()
                .into_iter()
                .filter(|c| !in_any(c, boxes))
                .collect()
        })
        .collect();
    let lines_per_page: Vec<Vec<Line>> = chunks_per_page
        .iter()
        .map(|cs| reconstruct_lines(cs))
        .collect();
    let hf = detect_header_footer(&doc.pages, &lines_per_page);
    let body = body_font_size(doc);

    let mut pages_blocks: Vec<Vec<Block>> = lines_per_page
        .into_iter()
        .zip(&doc.pages)
        .map(|(lines, page)| {
            let body_lines: Vec<Line> = lines.into_iter().filter(|l| !hf.is_running(l)).collect();
            let right = body_lines.iter().map(|l| l.x1).fold(0.0f32, f32::max);
            let fill_x = right - page.width.max(1.0) * 0.05;
            dehyphenate_blocks(group_blocks(&body_lines, body, fill_x))
        })
        .collect();
    assign_heading_levels(&mut pages_blocks);
    pages_blocks
}

/// Assign heading levels document-wide (G9c): tagged levels are kept; the
/// remaining headings get tiers by font size — distinct sizes (0.5pt buckets)
/// sorted descending map to levels 1..=3 (deeper tiers all become 3).
fn assign_heading_levels(pages: &mut [Vec<Block>]) {
    let mut sizes: Vec<i32> = pages
        .iter()
        .flatten()
        .filter(|b| b.heading && b.level == 0)
        .map(|b| (b.size * 2.0).round() as i32)
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes.dedup();
    for b in pages.iter_mut().flatten() {
        if b.heading && b.level == 0 {
            let key = (b.size * 2.0).round() as i32;
            let tier = sizes.iter().position(|&s| s == key).unwrap_or(0);
            b.level = (tier as u8 + 1).min(3);
        }
    }
}

/// Join consecutive blocks across a soft line-break hyphen, even when they did
/// not merge into one paragraph (e.g. left column of a 2-column page, where the
/// fill heuristic keeps lines separate). NID is word-level, so rejoining the
/// hyphen is what matters: "...com-" + "pact..." → "...compact...".
fn dehyphenate_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for b in blocks {
        let join = out.last().is_some_and(|p| {
            !p.heading
                && !b.heading
                && p.text.ends_with('-')
                && p.text
                    .chars()
                    .rev()
                    .nth(1)
                    .is_some_and(|c| c.is_alphabetic())
                && b.text.chars().next().is_some_and(|c| c.is_lowercase())
        });
        if join {
            let p = out.last_mut().unwrap();
            p.text.pop();
            p.text.push_str(&b.text);
            p.bbox = BBox {
                x0: p.bbox.x0.min(b.bbox.x0),
                y0: p.bbox.y0.min(b.bbox.y0),
                x1: p.bbox.x1.max(b.bbox.x1),
                y1: p.bbox.y1.max(b.bbox.y1),
            };
        } else {
            out.push(b);
        }
    }
    out
}

/// Body font size: the most common chunk size (mode, in 0.5 pt bins). More
/// robust than the median for heading detection — a doc with many headings
/// inflates the median, but body text is still the *most frequent* size.
/// Deterministic tie-break: highest count, then smallest size.
pub fn body_font_size(doc: &Document) -> f32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for c in doc.pages.iter().flat_map(|p| p.text_chunks()) {
        *counts
            .entry((c.font_size * 2.0).round() as u32)
            .or_insert(0) += 1;
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
        Line {
            text: text.into(),
            size,
            cy,
            x0: 0.0,
            x1,
            page: 1,
            bold: false,
            mono: false,
            form: false,
            tag_level: None,
            tagged_body: false,
        }
    }

    // fill_x = 90: lines reaching x1≈100 count as wrapped prose.
    const FILL: f32 = 90.0;

    #[test]
    fn paragraph_merges_close_lines_breaks_on_gap() {
        let lines = vec![
            line("First line of para", 10.0, 200.0),
            line("second line continues", 10.0, 188.0), // gap 12 < 18, fills → merge
            line("A new paragraph", 10.0, 150.0),       // gap 38 > 18 → break
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "First line of para second line continues");
        assert_eq!(blocks[1].text, "A new paragraph");
    }

    #[test]
    fn reading_groups_override_macro_order() {
        // Two chunks; geometry says A (top) before B, groups say B first.
        let mut a = TextChunk {
            text: "A".into(),
            bbox: BBox {
                x0: 0.0,
                y0: 90.0,
                x1: 20.0,
                y1: 100.0,
            },
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: Some(1),
            tag: None,
        };
        let mut b = a.clone();
        b.text = "B".into();
        b.bbox = BBox {
            x0: 0.0,
            y0: 10.0,
            x1: 20.0,
            y1: 20.0,
        };
        b.group = Some(0);
        let lines = reconstruct_lines(&[&a, &b]);
        assert_eq!(lines[0].text, "B", "group 0 must come first");
        assert_eq!(lines[1].text, "A");
        // Without groups, geometry wins.
        a.group = None;
        b.group = None;
        let lines = reconstruct_lines(&[&a, &b]);
        assert_eq!(lines[0].text, "A");
    }

    #[test]
    fn mono_runs_become_code_blocks_with_indent() {
        // Three monospace lines, the middle one indented by ~4 char cells
        // (x0 = 4 * 0.5em * size = 20 for size 10).
        let mk = |text: &str, cy: f32, x0: f32| Line {
            text: text.into(),
            size: 10.0,
            cy,
            x0,
            x1: x0 + 60.0,
            page: 1,
            bold: false,
            mono: true,
            form: false,
            tag_level: None,
            tagged_body: false,
        };
        let lines = vec![
            mk("fn main() {", 100.0, 0.0),
            mk("let x = 1;", 88.0, 20.0),
            mk("}", 76.0, 0.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 1, "mono run groups into one block");
        assert!(blocks[0].code);
        assert!(!blocks[0].heading);
        assert_eq!(blocks[0].text, "fn main() {\n    let x = 1;\n}");
        // A prose line is unaffected.
        let prose = vec![line("Just a sentence.", 10.0, 50.0)];
        assert!(!group_blocks(&prose, 10.0, FILL)[0].code);
    }

    #[test]
    fn vertical_marginalia_is_excluded() {
        let normal = TextChunk {
            text: "hello world".into(),
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 50.0,
                y1: 10.0,
            },
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        };
        let stamp = TextChunk {
            text: "arXiv:1234".into(),
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 5.0,
                y1: 40.0,
            }, // tall, narrow
            font_size: 10.0,
            font: None,
            page: 1,
            confidence: 1.0,
            bold: false,
            hidden: false,
            source: None,
            group: None,
            tag: None,
        };
        let lines = reconstruct_lines(&[&normal, &stamp]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "hello world");
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
    fn soft_hyphen_rejoined_across_lines() {
        let lines = vec![
            line("a wrapped line ending in com-", 10.0, 200.0),
            line("pact words continuing here", 10.0, 188.0),
        ];
        let blocks = group_blocks(&lines, 10.0, FILL);
        assert_eq!(blocks.len(), 1);
        assert!(
            blocks[0].text.contains("compact"),
            "got: {}",
            blocks[0].text
        );
        assert!(!blocks[0].text.contains("com-"), "hyphen removed");
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
                    bbox: BBox {
                        x0: 0.0,
                        y0: cy - 5.0,
                        x1: 50.0,
                        y1: cy + 5.0,
                    },
                    font_size: 10.0,
                    font: None,
                    page: number,
                    confidence: 1.0,
                    bold: false,
                    hidden: false,
                    source: None,
                    group: None,
                    tag: None,
                })
            })
            .collect();
        Page {
            number,
            width: 200.0,
            height,
            elements,
        }
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
        let lpp: Vec<Vec<Line>> = pages
            .iter()
            .map(|p| reconstruct_lines(&p.text_chunks()))
            .collect();
        let hf = detect_header_footer(&pages, &lpp);
        // "Page #" (digits folded) should be flagged; body should not.
        assert!(hf.is_running(&line("Page 1", 10.0, 10.0)));
        assert!(!hf.is_running(&line("Unique body of page", 10.0, 400.0)));
    }

    #[test]
    fn single_page_has_no_running_content() {
        let pages = vec![page_with_lines(1, &[("Footer", 10.0)], 800.0)];
        let lpp: Vec<Vec<Line>> = pages
            .iter()
            .map(|p| reconstruct_lines(&p.text_chunks()))
            .collect();
        let hf = detect_header_footer(&pages, &lpp);
        assert!(!hf.is_running(&line("Footer", 10.0, 10.0)));
    }
}

#[cfg(test)]
mod level_tests {
    use super::*;

    fn hblock(text: &str, size: f32, level: u8) -> Block {
        Block {
            text: text.into(),
            size,
            heading: true,
            level,
            code: false,
            page: 1,
            bbox: BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
        }
    }

    #[test]
    fn size_tiers_become_levels_and_tags_are_kept() {
        let mut pages = vec![vec![
            hblock("Chapter", 20.0, 0),
            hblock("Section", 16.0, 0),
            hblock("Sub", 13.0, 0),
            hblock("Deep", 11.0, 0),
            hblock("Tagged", 11.0, 2), // author-declared level survives
        ]];
        assign_heading_levels(&mut pages);
        let levels: Vec<u8> = pages[0].iter().map(|b| b.level).collect();
        assert_eq!(levels, vec![1, 2, 3, 3, 2], "tiers cap at 3, tags kept");
    }
}
