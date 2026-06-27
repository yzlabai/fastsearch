//! Simple-font encoding: turn a character code into a glyph name, and a glyph
//! name into Unicode text.
//!
//! This is what veraPDF's `org.verapdf.pd.font.Encoding` + `AdobeGlyphList`
//! provide for simple (Type1/TrueType) fonts that lack a `ToUnicode` CMap:
//! resolve `code -> glyph name` via a base encoding overlaid with the font's
//! `/Differences`, then `glyph name -> Unicode` via the Adobe Glyph List.
//! Algorithm referenced from veraPDF; tables/data are Adobe's (see
//! `resources/glyphlist/AdobeGlyphList.txt` and `encoding_tables.rs`),
//! independently parsed here.

use crate::encoding_tables::{MACROMAN, STANDARD, WINANSI};
use std::collections::HashMap;
use std::sync::OnceLock;

/// One of the predefined base encodings a `/Encoding` name can select.
pub fn base_table(name: &str) -> &'static [&'static str; 256] {
    match name {
        "WinAnsiEncoding" => &WINANSI,
        "MacRomanEncoding" => &MACROMAN,
        // StandardEncoding, or any unrecognized name, defaults to Standard.
        _ => &STANDARD,
    }
}

/// Adobe Glyph List: glyph name -> Unicode string. Parsed once from the
/// embedded resource (space-delimited `name HEX[ HEX...]`).
fn agl() -> &'static HashMap<&'static str, String> {
    static AGL: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    AGL.get_or_init(|| {
        let raw = include_str!("../resources/glyphlist/AdobeGlyphList.txt");
        let mut map = HashMap::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split_whitespace();
            let Some(name) = it.next() else { continue };
            let mut s = String::new();
            for hex in it {
                if let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    s.push(c);
                }
            }
            if !s.is_empty() {
                map.insert(name, s);
            }
        }
        map
    })
}

/// f-ligatures we deliberately decompose to ASCII so extracted text is
/// searchable (`fi` not U+FB01). Applied before the AGL lookup.
fn ligature(name: &str) -> Option<&'static str> {
    Some(match name {
        "fi" => "fi",
        "fl" => "fl",
        "ff" => "ff",
        "ffi" => "ffi",
        "ffl" => "ffl",
        _ => return None,
    })
}

/// Map a glyph name to Unicode text. Order: f-ligature decomposition, AGL,
/// then algorithmic `uniXXXX` / `uXXXX..` names (PDF/AGL convention).
/// Returns `None` for unmappable names (e.g. `g123`, `cidXXXX`).
pub fn glyph_to_unicode(name: &str) -> Option<String> {
    // `.notdef` is "no glyph"; the AGL lists it as U+0000 which we must not emit.
    if name == ".notdef" || name.is_empty() {
        return None;
    }
    if let Some(s) = ligature(name) {
        return Some(s.to_string());
    }
    if let Some(s) = agl().get(name) {
        return Some(s.clone());
    }
    // `uniXXXX` (one or more 4-hex BMP scalars) or `uXXXXXX` (4–6 hex).
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() >= 4 && hex.len() % 4 == 0 {
            let s: String = hex
                .as_bytes()
                .chunks(4)
                .filter_map(|c| std::str::from_utf8(c).ok())
                .filter_map(|h| u32::from_str_radix(h, 16).ok())
                .filter_map(char::from_u32)
                .collect();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                return Some(c.to_string());
            }
        }
    }
    // Strip a glyph-name suffix like `name.sc` / `a.alt` and retry once.
    if let Some((base, _)) = name.split_once('.') {
        if base != name && !base.is_empty() {
            return glyph_to_unicode(base);
        }
    }
    None
}

/// Build a simple font's `code -> glyph name` table: a base encoding overlaid
/// with `/Differences`. `base` is the chosen 256-entry predefined table;
/// `differences` is the raw `/Differences` array entries (code resets as
/// integers, glyph names as names) already extracted from the font dict.
pub fn build_encoding(base: &[&'static str; 256], differences: &[Diff]) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = base
        .iter()
        .map(|n| (!n.is_empty()).then(|| n.to_string()))
        .collect();
    let mut code = 0usize;
    for d in differences {
        match d {
            Diff::Code(c) => code = *c as usize,
            Diff::Name(n) => {
                if code < 256 {
                    out[code] = Some(n.clone());
                    code += 1;
                }
            }
        }
    }
    out
}

/// A `/Differences` array entry.
pub enum Diff {
    Code(u32),
    Name(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agl_basics() {
        assert_eq!(glyph_to_unicode("space").as_deref(), Some(" "));
        assert_eq!(glyph_to_unicode("eacute").as_deref(), Some("é"));
        assert_eq!(glyph_to_unicode("A").as_deref(), Some("A"));
    }

    #[test]
    fn ligatures_decompose_to_ascii() {
        assert_eq!(glyph_to_unicode("fi").as_deref(), Some("fi"));
        assert_eq!(glyph_to_unicode("ffl").as_deref(), Some("ffl"));
    }

    #[test]
    fn algorithmic_uni_names() {
        assert_eq!(glyph_to_unicode("uni0041").as_deref(), Some("A"));
        assert_eq!(glyph_to_unicode("uni004100420043").as_deref(), Some("ABC"));
        assert!(glyph_to_unicode("u1F600").is_some());
    }

    #[test]
    fn unmappable_returns_none() {
        assert_eq!(glyph_to_unicode("g123"), None);
        assert_eq!(glyph_to_unicode(".notdef"), None);
    }

    #[test]
    fn differences_override_base() {
        let enc = build_encoding(
            &STANDARD,
            &[
                Diff::Code(65),
                Diff::Name("fi".into()),
                Diff::Name("fl".into()),
            ],
        );
        // 65/66 overridden, 67 still Standard 'C'.
        assert_eq!(enc[65].as_deref(), Some("fi"));
        assert_eq!(enc[66].as_deref(), Some("fl"));
        assert_eq!(enc[67].as_deref(), Some("C"));
    }

    #[test]
    fn dotted_suffix_falls_back_to_base_name() {
        assert_eq!(glyph_to_unicode("a.sc").as_deref(), Some("a"));
    }
}
