//! LaTeX *source* backend (G1b long-tail formats): the common article subset,
//! hand-rolled and line-oriented — no dependency, no TeX engine.
//!
//! Scope (documented bounds, not a TeX implementation):
//! - `\title`/`\author` + `\maketitle`, `\section{}`…`\subsubsection{}`,
//!   `\begin{abstract}`;
//! - paragraphs split on blank lines; inline `\textbf`/`\emph`/… unwrapped,
//!   `\cite`/`\ref`/`\label`/`\footnote` dropped, escapes (`\%` `\&` …)
//!   unescaped, `~` → space;
//! - `itemize`/`enumerate` → one marker-prefixed paragraph per `\item`
//!   (nesting flattened);
//! - `tabular` → a real [`Table`] (rows on `\\`, cells on `&`);
//! - `figure`/`table` envs contribute their `\caption` (as `Figure: …`) and
//!   any inner `tabular`; other content inside them is skipped;
//! - math (`$…$` inline, `equation`/`align`/`displaymath` blocks) is kept
//!   VERBATIM — raw TeX math is the most faithful text form of the source;
//! - `verbatim` lines pass through untouched.
//!
//! NOT handled (silently skipped or passed through as text, never an error):
//! `\input`/`\include` (no file traversal — one file is one document),
//! custom macros, bibliographies, `\def`, TikZ. Real arXiv sources parse to
//! readable structure; exotic preambles degrade to plain paragraphs.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use std::path::Path;

pub struct TexParser;

impl DocumentParser for TexParser {
    fn name(&self) -> &'static str {
        "latex"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tex"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let text = docparse_core::textio::read_text(path)?;
        let mut doc = parse_str(&text);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

const BODY_SIZE: f32 = 10.0;
const TITLE_SIZE: f32 = 18.0;
/// `\section` / `\subsection` / `\subsubsection` font sizes — all > 1.25 ×
/// body so the core heading rule classifies them, tiered for level assignment.
const SECTION_SIZES: [f32; 3] = [16.0, 14.0, 12.6];

/// Parse LaTeX source text into a [`Document`].
pub fn parse_str(src: &str) -> Document {
    let stripped = strip_comments(src);
    // Title/author live in the preamble; body is inside the document env
    // (a bare fragment without \begin{document} is parsed whole).
    let title = brace_arg_after(&stripped, "\\title");
    let author = brace_arg_after(&stripped, "\\author");
    let body = match stripped.find("\\begin{document}") {
        Some(i) => {
            let after = &stripped[i + "\\begin{document}".len()..];
            after.split("\\end{document}").next().unwrap_or(after)
        }
        None => &stripped,
    };

    let mut b = PageBuilder::letter();
    let mut para = String::new();
    let mut item: Option<String> = None; // current \item text, marker included
    let mut enum_counters: Vec<Option<usize>> = Vec::new(); // None = itemize
    let mut skip_env = 0usize; // inside figure/table: only caption + tabular
    let mut decl_depth = 0i32; // open braces of a skipped multi-line declaration
    let mut tabular: Option<String> = None;
    let mut math: Option<String> = None;
    let mut verbatim = false;

    let flush = |b: &mut PageBuilder, buf: &mut String| {
        let text = clean_inline(buf.trim());
        if !text.is_empty() {
            b.paragraph(text, BODY_SIZE);
        }
        buf.clear();
    };

    for line in body.lines() {
        let t = line.trim();

        if verbatim {
            if t.starts_with("\\end{verbatim}") {
                verbatim = false;
            } else {
                b.paragraph(line.to_string(), BODY_SIZE);
            }
            continue;
        }
        if let Some(acc) = &mut math {
            if t.starts_with("\\end{") {
                let text = acc.trim().to_string();
                if !text.is_empty() {
                    b.paragraph(text, BODY_SIZE);
                }
                math = None;
            } else {
                acc.push_str(line);
                acc.push(' ');
            }
            continue;
        }
        if let Some(acc) = &mut tabular {
            if t.starts_with("\\end{tabular}") {
                let rows = parse_tabular(acc);
                if !rows.is_empty() {
                    b.table(rows, BODY_SIZE);
                }
                tabular = None;
            } else {
                acc.push_str(line);
                acc.push('\n');
            }
            continue;
        }

        if t.is_empty() {
            if let Some(it) = item.take() {
                b.list_item(clean_inline(it.trim()), BODY_SIZE);
            }
            flush(&mut b, &mut para);
            continue;
        }

        if let Some(rest) = t.strip_prefix("\\begin{") {
            let env = rest.split('}').next().unwrap_or("");
            match env {
                "itemize" => {
                    flush(&mut b, &mut para);
                    enum_counters.push(None);
                }
                "enumerate" => {
                    flush(&mut b, &mut para);
                    enum_counters.push(Some(0));
                }
                "tabular" | "tabular*" => {
                    flush(&mut b, &mut para);
                    // Drop the column spec on the same line if present.
                    let after = t.split_once('}').map(|(_, r)| r).unwrap_or("");
                    let after = after.trim_start().trim_start_matches('{');
                    let after = after.split_once('}').map(|(_, r)| r).unwrap_or(after);
                    tabular = Some(format!("{after}\n"));
                }
                "verbatim" => {
                    flush(&mut b, &mut para);
                    verbatim = true;
                }
                "equation" | "equation*" | "align" | "align*" | "displaymath" | "eqnarray" => {
                    flush(&mut b, &mut para);
                    math = Some(String::new());
                }
                "abstract" => {
                    flush(&mut b, &mut para);
                    b.paragraph("Abstract".to_string(), SECTION_SIZES[0]);
                }
                "figure" | "figure*" | "table" | "table*" => {
                    flush(&mut b, &mut para);
                    skip_env += 1;
                }
                _ => {} // unknown env: keep reading its text lines as prose
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("\\end{") {
            let env = rest.split('}').next().unwrap_or("");
            match env {
                "itemize" | "enumerate" => {
                    if let Some(it) = item.take() {
                        b.list_item(clean_inline(it.trim()), BODY_SIZE);
                    }
                    enum_counters.pop();
                }
                "figure" | "figure*" | "table" | "table*" => {
                    skip_env = skip_env.saturating_sub(1);
                }
                _ => {}
            }
            continue;
        }

        // Inside figure/table environments only captions (and tabular,
        // handled above) carry over; graphics commands are skipped.
        if skip_env > 0 {
            if let Some(cap) = brace_arg_after(t, "\\caption") {
                b.paragraph(format!("Figure: {}", clean_inline(&cap)), BODY_SIZE);
            }
            continue;
        }

        if let Some((level, heading)) = section_of(t) {
            if let Some(it) = item.take() {
                b.list_item(clean_inline(it.trim()), BODY_SIZE);
            }
            flush(&mut b, &mut para);
            b.paragraph(clean_inline(&heading), SECTION_SIZES[level]);
            continue;
        }
        if t.starts_with("\\maketitle") {
            if let Some(ti) = &title {
                b.paragraph(clean_inline(ti), TITLE_SIZE);
            }
            if let Some(au) = &author {
                b.paragraph(clean_inline(au), BODY_SIZE);
            }
            continue;
        }
        // Title-page declarations may sit inside the document body (common
        // in conference templates); their values surface via \maketitle, the
        // raw declaration lines must not leak as prose. \keywords carries
        // real content, so it surfaces inline.
        if let Some(kw) = t
            .strip_prefix("\\keywords")
            .and_then(|_| brace_arg_after(t, "\\keywords"))
        {
            flush(&mut b, &mut para);
            b.paragraph(format!("Keywords: {}", clean_inline(&kw)), BODY_SIZE);
            continue;
        }
        const DECL_LINES: [&str; 7] = [
            "\\title",
            "\\author",
            "\\date",
            "\\institute",
            "\\authorrunning",
            "\\titlerunning",
            "\\email",
        ];
        if decl_depth > 0 || DECL_LINES.iter().any(|d| t.starts_with(d)) {
            // Declarations span lines (multi-author \author{...}); track brace
            // balance so continuation lines are skipped too.
            decl_depth += brace_balance(t);
            decl_depth = decl_depth.max(0);
            continue;
        }
        if let Some(rest) = t.strip_prefix("\\item") {
            if let Some(it) = item.take() {
                b.list_item(clean_inline(it.trim()), BODY_SIZE);
            }
            let marker = match enum_counters.last_mut() {
                Some(Some(n)) => {
                    *n += 1;
                    format!("{n}. ")
                }
                _ => "• ".to_string(),
            };
            item = Some(format!("{marker}{}", rest.trim_start_matches('m').trim()));
            continue;
        }
        // Lone layout/preamble-ish commands contribute nothing.
        if t.starts_with('\\') && !t.contains(' ') && !t.contains('$') {
            continue;
        }

        match &mut item {
            Some(it) => {
                it.push(' ');
                it.push_str(t);
            }
            None => {
                para.push_str(t);
                para.push(' ');
            }
        }
    }
    if let Some(it) = item.take() {
        b.list_item(clean_inline(it.trim()), BODY_SIZE);
    }
    flush(&mut b, &mut para);

    Document {
        source: "<latex>".to_string(),
        provenance: Some(Provenance::new("latex", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    }
}

/// `\section{...}` family → (tier index, heading text). Starred forms too.
fn section_of(line: &str) -> Option<(usize, String)> {
    for (i, name) in ["\\section", "\\subsection", "\\subsubsection"]
        .iter()
        .enumerate()
        .rev()
    // longest prefix first: \subsubsection before \subsection before \section
    {
        if let Some(rest) = line.strip_prefix(name) {
            let rest = rest.strip_prefix('*').unwrap_or(rest);
            return balanced_brace_arg(rest).map(|a| (i, a));
        }
    }
    None
}

/// First balanced `{...}` argument following `cmd` anywhere in `text`.
fn brace_arg_after(text: &str, cmd: &str) -> Option<String> {
    let i = text.find(cmd)?;
    balanced_brace_arg(&text[i + cmd.len()..])
}

/// Leading (after optional whitespace) balanced `{...}` group's content.
fn balanced_brace_arg(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let mut chars = rest.char_indices();
    let (_, '{') = chars.next()? else { return None };
    let mut depth = 1;
    for (i, c) in chars {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[1..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Net `{`/`}` balance of a line (escaped braces excluded).
fn brace_balance(line: &str) -> i32 {
    let mut depth = 0i32;
    let mut prev = '\0';
    for c in line.chars() {
        match c {
            '{' if prev != '\\' => depth += 1,
            '}' if prev != '\\' => depth -= 1,
            _ => {}
        }
        prev = c;
    }
    depth
}

/// Strip `%` comments (respecting the `\%` escape), line by line.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'%' && (i == 0 || bytes[i - 1] != b'\\') {
                cut = i;
                break;
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Unwrap/drop the common inline commands. Repeated passes handle one level
/// of nesting per pass; runaway input is bounded by the pass cap.
fn clean_inline(text: &str) -> String {
    const UNWRAP: [&str; 8] = [
        "\\textbf",
        "\\textit",
        "\\emph",
        "\\texttt",
        "\\textsc",
        "\\underline",
        "\\url",
        "\\mbox",
    ];
    const DROP: [&str; 8] = [
        "\\cite",
        "\\ref",
        "\\eqref",
        "\\label",
        "\\footnote",
        "\\autoref",
        "\\orcidID",
        "\\thanks",
    ];
    let mut s = text.to_string();
    for _ in 0..4 {
        let before = s.clone();
        for cmd in UNWRAP {
            while let Some(i) = s.find(cmd) {
                let Some(arg) = balanced_brace_arg(&s[i + cmd.len()..]) else {
                    break;
                };
                let len = cmd.len() + arg_span(&s[i + cmd.len()..]);
                s.replace_range(i..i + len, &arg);
            }
        }
        for cmd in DROP {
            while let Some(i) = s.find(cmd) {
                let tail = &s[i + cmd.len()..];
                // Optional [..] then {..}.
                let mut off = 0usize;
                let t2 = tail.trim_start();
                off += tail.len() - t2.len();
                let t3 = if t2.starts_with('[') {
                    match t2.find(']') {
                        Some(j) => {
                            off += j + 1;
                            &t2[j + 1..]
                        }
                        None => t2,
                    }
                } else {
                    t2
                };
                if balanced_brace_arg(t3).is_some() {
                    off += arg_span(t3);
                }
                s.replace_range(i..i + cmd.len() + off, "");
            }
        }
        if s == before {
            break;
        }
    }
    let s = s
        .replace("\\and", ", ")
        .replace("\\&", "&")
        .replace("\\%", "%")
        .replace("\\$", "$")
        .replace("\\_", "_")
        .replace("\\#", "#")
        .replace("\\\\", " ")
        .replace('~', " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte length of the leading whitespace + balanced `{...}` group.
fn arg_span(rest: &str) -> usize {
    let trimmed = rest.trim_start();
    let lead = rest.len() - trimmed.len();
    let mut depth = 0usize;
    for (i, c) in trimmed.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return lead + i + 1;
                }
            }
            _ if depth == 0 => return lead, // no group follows
            _ => {}
        }
    }
    lead
}

/// `tabular` body → row-major cell text (`\\` rows, `&` cells); rules
/// (`\hline`, booktabs) dropped.
fn parse_tabular(body: &str) -> Vec<Vec<String>> {
    body.split("\\\\")
        .map(|row| {
            row.split('&')
                .map(|c| {
                    clean_inline(
                        c.replace("\\hline", "")
                            .replace("\\toprule", "")
                            .replace("\\midrule", "")
                            .replace("\\bottomrule", "")
                            .trim(),
                    )
                })
                .collect::<Vec<String>>()
        })
        .filter(|cells: &Vec<String>| cells.iter().any(|c| !c.is_empty()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::Element;

    fn texts(doc: &Document) -> Vec<(String, f32)> {
        doc.pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some((t.text.clone(), t.font_size)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sections_title_lists_and_inline() {
        let doc = parse_str(
            "\\documentclass{article}\n\\title{My Paper}\n\\author{Ada}\n\\begin{document}\n\\maketitle\n\n\\section{Intro}\nHello \\textbf{bold} and \\emph{em}~world\\cite{x}.\n\n\\begin{itemize}\n\\item alpha\n\\item beta\n\\end{itemize}\n\n\\begin{enumerate}\n\\item one\n\\item two\n\\end{enumerate}\n\\end{document}\n",
        );
        let t = texts(&doc);
        assert_eq!(t[0], ("My Paper".to_string(), TITLE_SIZE));
        assert_eq!(t[1].0, "Ada");
        assert_eq!(t[2], ("Intro".to_string(), SECTION_SIZES[0]));
        assert_eq!(t[3].0, "Hello bold and em world.");
        assert_eq!(t[4].0, "• alpha");
        assert_eq!(t[6].0, "1. one");
        assert_eq!(t[7].0, "2. two");
    }

    #[test]
    fn tabular_becomes_table_and_caption_carries() {
        let doc = parse_str(
            "\\begin{document}\n\\begin{table}\n\\caption{Results \\textit{here}}\n\\begin{tabular}{ll}\nA & B \\\\\n1 & 2 \\\\\n\\end{tabular}\n\\end{table}\n\\end{document}\n",
        );
        let tables: Vec<_> = doc
            .pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows[0][0].text, "A");
        assert_eq!(tables[0].rows[1][1].text, "2");
        assert!(texts(&doc).iter().any(|(s, _)| s == "Figure: Results here"));
    }

    #[test]
    fn comments_math_and_subsection_levels() {
        let doc = parse_str(
            "\\begin{document}\n% a comment\nkeep 50\\% of it\n\n\\subsection{Deep}\n\\begin{equation}\nE = mc^2\n\\end{equation}\n\\end{document}\n",
        );
        let t = texts(&doc);
        assert_eq!(t[0].0, "keep 50% of it");
        assert_eq!(t[1], ("Deep".to_string(), SECTION_SIZES[1]));
        assert_eq!(t[2].0, "E = mc^2");
    }

    // --- pure-helper unit tests (branches the integration tests don't reach) ---

    #[test]
    fn balanced_brace_arg_handles_nesting_and_failure() {
        assert_eq!(balanced_brace_arg("{abc}").as_deref(), Some("abc"));
        assert_eq!(balanced_brace_arg("  {a{b}c}").as_deref(), Some("a{b}c"));
        assert_eq!(balanced_brace_arg("noargs"), None);
        assert_eq!(balanced_brace_arg("{unclosed"), None);
        assert_eq!(balanced_brace_arg(""), None);
    }

    #[test]
    fn brace_arg_after_finds_command_anywhere() {
        assert_eq!(
            brace_arg_after("pre \\title{X} post", "\\title").as_deref(),
            Some("X")
        );
        assert_eq!(brace_arg_after("no command here", "\\title"), None);
    }

    #[test]
    fn section_of_levels_and_star_variants() {
        assert_eq!(section_of("\\section{Intro}"), Some((0, "Intro".into())));
        assert_eq!(section_of("\\subsection{Deep}"), Some((1, "Deep".into())));
        assert_eq!(section_of("\\subsubsection{D}"), Some((2, "D".into())));
        // \section* (unnumbered) strips the star.
        assert_eq!(section_of("\\section*{Star}"), Some((0, "Star".into())));
        // Longest-prefix-first: \subsection must not be read as \section.
        assert_eq!(section_of("\\subsection{X}").map(|(l, _)| l), Some(1));
        assert_eq!(section_of("\\paragraph{x}"), None);
    }

    #[test]
    fn brace_balance_respects_escapes() {
        assert_eq!(brace_balance("{a}"), 0);
        assert_eq!(brace_balance("{{"), 2);
        assert_eq!(brace_balance("a}"), -1);
        assert_eq!(brace_balance("\\{ \\}"), 0); // escaped braces ignored
        assert_eq!(brace_balance("x \\{ {"), 1);
    }

    #[test]
    fn strip_comments_keeps_escaped_percent() {
        assert_eq!(strip_comments("keep % drop\n"), "keep \n");
        assert_eq!(strip_comments("a\\% b % c\n"), "a\\% b \n");
        assert_eq!(strip_comments("% whole\n"), "\n");
    }

    #[test]
    fn clean_inline_unwraps_nests_drops_and_unescapes() {
        assert_eq!(clean_inline("\\textbf{x}"), "x");
        // Repeated passes unwrap one nesting level each.
        assert_eq!(clean_inline("\\textbf{\\emph{y}}"), "y");
        // \cite is dropped entirely; surrounding whitespace collapses.
        assert_eq!(clean_inline("a \\cite{ref} b"), "a b");
        // Drop commands skip an optional [..] before the {..}.
        assert_eq!(clean_inline("a\\footnote[2]{note} b"), "a b");
        // Escaped specials and ~ normalize.
        assert_eq!(clean_inline("50\\% off~now"), "50% off now");
        assert_eq!(clean_inline("\\and joins"), ", joins");
    }

    #[test]
    fn arg_span_measures_leading_group() {
        assert_eq!(arg_span("{abc}rest"), 5);
        assert_eq!(arg_span("  {ab}"), 6); // 2 lead + "{ab}"
        assert_eq!(arg_span("noarg"), 0); // no group follows
    }

    #[test]
    fn parse_tabular_splits_rows_and_drops_rules() {
        let rows = parse_tabular("\\hline A & B \\\\ \\midrule 1 & 2 \\\\");
        assert_eq!(rows, vec![vec!["A", "B"], vec!["1", "2"]]);
    }
}
