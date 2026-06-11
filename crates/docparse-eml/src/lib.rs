//! EML email backend (G1b long-tail formats), on `mail-parser` (pure Rust):
//! MIME multipart walking, quoted-printable/base64 decoding and charset
//! handling all come from the library — this backend only maps the decoded
//! message onto the synthetic layout.
//!
//! Mapping: `Subject` → document heading; `From`/`To`/`Date` → one metadata
//! line each; the first text body → paragraphs (split on blank lines). When
//! a message is HTML-only, mail-parser's `body_text` already provides its
//! text conversion. Attachments are LISTED (`[attachment] name (bytes)`) —
//! their content is not parsed (a future increment may route supported types
//! through the registry). Nested message/rfc822 parts are not descended into.

use docparse_core::ir::{Document, Provenance};
use docparse_core::parser::DocumentParser;
use docparse_core::synth::PageBuilder;
use mail_parser::{MessageParser, MimeHeaders};
use std::path::Path;

pub struct EmlParser;

impl DocumentParser for EmlParser {
    fn name(&self) -> &'static str {
        "eml"
    }

    fn supports(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("eml"))
            .unwrap_or(false)
    }

    fn parse(&self, path: &Path) -> anyhow::Result<Document> {
        let bytes = std::fs::read(path)?;
        let mut doc = parse_bytes(&bytes)?;
        doc.source = path.display().to_string();
        Ok(doc)
    }
}

const BODY_SIZE: f32 = 10.0;
const SUBJECT_SIZE: f32 = 16.0;

/// Parse raw RFC-5322 bytes into a [`Document`].
pub fn parse_bytes(bytes: &[u8]) -> anyhow::Result<Document> {
    let msg = MessageParser::default()
        .parse(bytes)
        .ok_or_else(|| anyhow::anyhow!("not a parseable RFC-5322 message"))?;

    let mut b = PageBuilder::letter();
    if let Some(subject) = msg.subject() {
        b.paragraph(subject.to_string(), SUBJECT_SIZE);
    }
    for (label, value) in [
        ("From", address_line(msg.from())),
        ("To", address_line(msg.to())),
        ("Date", msg.date().map(|d| d.to_rfc3339())),
    ] {
        if let Some(v) = value {
            b.paragraph(format!("{label}: {v}"), BODY_SIZE);
        }
    }

    // First text body; mail-parser converts HTML-only mails to text here.
    if let Some(text) = msg.body_text(0) {
        let text = text.replace("\r\n", "\n");
        for para in text.split("\n\n") {
            let para = para
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join(" ");
            b.paragraph(para.trim().to_string(), BODY_SIZE);
        }
    }

    for part in msg.attachments() {
        let name = part.attachment_name().unwrap_or("(unnamed)").to_string();
        b.paragraph(
            format!("[attachment] {name} ({} bytes)", part.contents().len()),
            BODY_SIZE,
        );
    }

    Ok(Document {
        source: "<eml>".to_string(),
        provenance: Some(Provenance::new("eml", env!("CARGO_PKG_VERSION"))),
        pages: b.finish(),
    })
}

/// "Name <addr>" lines for From/To headers (first few, comma-joined).
fn address_line(addr: Option<&mail_parser::Address<'_>>) -> Option<String> {
    let list: Vec<String> = addr?
        .iter()
        .take(8)
        .map(|a| match (a.name(), a.address()) {
            (Some(n), Some(ad)) => format!("{n} <{ad}>"),
            (None, Some(ad)) => ad.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then(|| list.join(", "))
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
    fn plain_mail_maps_subject_headers_body() {
        let raw = b"From: Ada Lovelace <ada@example.org>\r\n\
To: Charles <charles@example.org>\r\n\
Subject: Engine notes\r\n\
Date: Tue, 10 Jun 2026 09:00:00 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
First paragraph of the note.\r\n\
\r\n\
Second paragraph here.\r\n";
        let doc = parse_bytes(raw).unwrap();
        let t = texts(&doc);
        assert_eq!(t[0], "Engine notes");
        assert!(t[1].starts_with("From: Ada Lovelace <ada@example.org>"));
        assert!(t.iter().any(|s| s == "First paragraph of the note."));
        assert!(t.iter().any(|s| s == "Second paragraph here."));
    }

    #[test]
    fn multipart_html_only_and_attachment() {
        let raw = b"From: a@b.c\r\n\
Subject: =?utf-8?B?5L2g5aW9?=\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=XY\r\n\
\r\n\
--XY\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Hello <b>world</b></p></body></html>\r\n\
--XY\r\n\
Content-Type: application/pdf; name=\"r.pdf\"\r\n\
Content-Disposition: attachment; filename=\"r.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQ=\r\n\
--XY--\r\n";
        let doc = parse_bytes(raw).unwrap();
        let t = texts(&doc);
        assert_eq!(t[0], "你好"); // RFC 2047 encoded-word decoded
        assert!(t.iter().any(|s| s.contains("Hello world"))); // HTML → text
        assert!(t.iter().any(|s| s.starts_with("[attachment] r.pdf")));
    }
}
