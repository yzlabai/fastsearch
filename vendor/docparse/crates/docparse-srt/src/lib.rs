//! Subtitle backend: SRT and WebVTT become timestamped paragraphs on the
//! synthetic layout (G1b long-tail formats). Hand-rolled parser — both
//! formats are line-oriented blocks separated by blank lines; no dependency
//! is warranted for something this small.
//!
//! Each cue renders as one paragraph `[hh:mm:ss] text` (start time only,
//! milliseconds dropped): the timestamp is what makes a subtitle citable
//! (jump to the moment in the media), so it stays in the text rather than
//! being thrown away. WebVTT `NOTE`/`STYLE`/`REGION` blocks are skipped;
//! inline tags (`<i>`, `<b>`, `<c.class>`) are stripped; voice spans
//! (`<v Speaker>`) become a `Speaker: ` prefix.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use std::path::Path;

pub struct SrtParser;

impl DocumentParser for SrtParser {
    fn name(&self) -> &'static str {
        "subtitle"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("srt") || e.eq_ignore_ascii_case("vtt"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let text = docparse_core::textio::read_text(path)?;
        let mut doc = parse_str(&text);
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

/// One parsed cue: start timestamp (display form) + flattened text.
struct Cue {
    start: String,
    text: String,
}

/// Parse SRT or WebVTT text into a [`Document`]. The two formats share the
/// block structure; the header line and timestamp separators differ.
pub fn parse_str(text: &str) -> Document {
    let mut b = PageBuilder::letter();
    for cue in parse_cues(text) {
        b.paragraph(format!("[{}] {}", cue.start, cue.text), 10.0);
    }
    Document {
        source: "<subtitle>".to_string(),
        provenance: Some(Provenance::new("subtitle", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    }
}

fn parse_cues(text: &str) -> Vec<Cue> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let lines: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        // WebVTT header and non-cue blocks.
        let head = lines[0];
        if head.starts_with("WEBVTT")
            || head.starts_with("NOTE")
            || head.starts_with("STYLE")
            || head.starts_with("REGION")
        {
            continue;
        }
        // The timestamp line is the one containing "-->"; anything before it
        // is an optional numeric index (SRT) or cue identifier (VTT).
        let Some(ts_idx) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let start_raw = lines[ts_idx].split("-->").next().unwrap_or("").trim();
        let start = normalize_timestamp(start_raw);
        let body = lines[ts_idx + 1..]
            .iter()
            .map(|l| strip_tags(l))
            .collect::<Vec<_>>()
            .join(" ");
        let body = body.trim().to_string();
        if !body.is_empty() {
            cues.push(Cue { start, text: body });
        }
    }
    cues
}

/// `00:00:01,000` (SRT) / `00:01.000` or `00:00:01.000` (VTT) → `hh:mm:ss`.
/// VTT may omit hours; pad so all cues align. Unparseable input passes
/// through as-is rather than being dropped (the cue text still matters).
fn normalize_timestamp(raw: &str) -> String {
    let no_ms = raw.split([',', '.']).next().unwrap_or(raw).trim();
    match no_ms.split(':').count() {
        2 => format!("00:{no_ms}"),
        _ => no_ms.to_string(),
    }
}

/// Drop `<...>` inline tags; a voice span `<v Speaker>` contributes a
/// `Speaker: ` prefix (the closing `</v>` just disappears).
fn strip_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            // Unclosed angle bracket: keep it literally.
            out.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let tag = &rest[open + 1..open + close];
        if let Some(speaker) = tag.strip_prefix("v ") {
            out.push_str(speaker.trim());
            out.push_str(": ");
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use docparse_core::ir::Element;

    fn texts(doc: &Document) -> Vec<String> {
        doc.pages
            .iter()
            .flat_map(|p| &p.elements)
            .filter_map(|e| match e {
                Element::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn srt_cues_become_timestamped_paragraphs() {
        let doc = parse_str("1\n00:00:01,000 --> 00:00:04,000\nHello\nworld\n\n2\n00:01:02,500 --> 00:01:03,000\nsecond cue\n");
        let t = texts(&doc);
        assert_eq!(t[0], "[00:00:01] Hello world");
        assert_eq!(t[1], "[00:01:02] second cue");
    }

    #[test]
    fn vtt_header_notes_and_tags() {
        let doc = parse_str(
            "WEBVTT\n\nNOTE a comment\n\n00:01.000 --> 00:04.000\n<v Alice>Hi <i>there</i>\n\nintro\n00:00:05.000 --> 00:00:06.000 align:start\nplain\n",
        );
        let t = texts(&doc);
        assert_eq!(t[0], "[00:00:01] Alice: Hi there");
        assert_eq!(t[1], "[00:00:05] plain");
    }

    #[test]
    fn crlf_and_malformed_blocks_survive() {
        let doc = parse_str(
            "1\r\n00:00:01,000 --> 00:00:02,000\r\ntext\r\n\r\njust prose no timestamp\r\n",
        );
        let t = texts(&doc);
        assert_eq!(t, vec!["[00:00:01] text"]);
    }
}
