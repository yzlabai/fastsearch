//! Minimal CMap support, ported from veraPDF's `pd.font.cmap` package.
//!
//! We parse the `ToUnicode` stream (a PostScript-flavored CMap) into:
//! - **codespace ranges** — used to split a show-string's bytes into variable
//!   length character codes (veraPDF's `CMap.getCodeFromStream` /
//!   `CodeSpace.contains`).
//! - **single mappings** (`beginbfchar`) and **range mappings**
//!   (`beginbfrange`) — code → Unicode string (veraPDF's `CMap.getUnicode` /
//!   `ToUnicodeInterval`).
//!
//! This is the piece opendataloader-pdf gets "for free" from veraPDF and the
//! reason `lorem.pdf` (an embedded subset CID font) previously decoded to
//! nothing.

/// An inclusive byte-range that defines how many bytes a code occupies.
/// Mirrors veraPDF `CodeSpace`.
#[derive(Debug, Clone)]
pub struct CodeSpace {
    pub begin: Vec<u8>,
    pub end: Vec<u8>,
}

impl CodeSpace {
    fn len(&self) -> usize {
        self.begin.len()
    }
    /// Whether `code` (same length as this range) lies within [begin, end] byte-wise.
    fn contains(&self, code: &[u8]) -> bool {
        if self.begin.len() != code.len() {
            return false;
        }
        for ((&b, &e), &c) in self.begin.iter().zip(&self.end).zip(code) {
            if c < b || c > e {
                return false;
            }
        }
        true
    }
}

/// A contiguous `beginbfrange` mapping: codes [begin, end] map to consecutive
/// Unicode values starting at `start`. Mirrors veraPDF `ToUnicodeInterval`.
#[derive(Debug, Clone)]
struct UnicodeInterval {
    begin: u32,
    end: u32,
    start: u32,
    width: usize,
}

impl UnicodeInterval {
    fn unicode(&self, code: u32) -> String {
        let value = code - self.begin + self.start;
        let mut bytes = vec![0u8; self.width];
        let mut v = value;
        for idx in (0..self.width).rev() {
            bytes[idx] = (v & 0xFF) as u8;
            v >>= 8;
        }
        utf16be_to_string(&bytes)
    }
}

/// A parsed CMap (we only use it for ToUnicode + codespace splitting).
#[derive(Debug, Default, Clone)]
pub struct CMap {
    pub codespaces: Vec<CodeSpace>,
    single: std::collections::HashMap<u32, String>,
    ranges: Vec<UnicodeInterval>,
}

impl CMap {
    /// Map a character code to its Unicode string, if known.
    pub fn unicode(&self, code: u32) -> Option<String> {
        if let Some(s) = self.single.get(&code) {
            return Some(s.clone());
        }
        for r in &self.ranges {
            if code >= r.begin && code <= r.end {
                return Some(r.unicode(code));
            }
        }
        None
    }

    /// Read the next character code from `data` at `pos`, returning (code, len).
    /// Ported from veraPDF `CMap.getCodeFromStream` (simplified): prefer the
    /// shortest codespace length that fully matches.
    pub fn next_code(&self, data: &[u8], pos: usize) -> (u32, usize) {
        for len in 1..=4usize {
            if pos + len > data.len() {
                break;
            }
            let slice = &data[pos..pos + len];
            if self
                .codespaces
                .iter()
                .any(|c| c.len() == len && c.contains(slice))
            {
                return (number_be(slice), len);
            }
        }
        // No full match — fall back to the shortest declared codespace length.
        let n = self
            .codespaces
            .iter()
            .map(|c| c.len())
            .min()
            .unwrap_or(1)
            .min(data.len().saturating_sub(pos))
            .max(1);
        (number_be(&data[pos..pos + n]), n)
    }

    /// Parse a ToUnicode CMap stream.
    pub fn parse(data: &[u8]) -> CMap {
        let toks = tokenize(data);
        let mut cmap = CMap::default();
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tok::Word(w) if w == "begincodespacerange" => {
                    i += 1;
                    while i < toks.len() && !is_word(&toks[i], "endcodespacerange") {
                        if let (Tok::Hex(lo), Some(Tok::Hex(hi))) = (&toks[i], toks.get(i + 1)) {
                            cmap.codespaces.push(CodeSpace {
                                begin: lo.clone(),
                                end: hi.clone(),
                            });
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                Tok::Word(w) if w == "beginbfchar" => {
                    i += 1;
                    while i < toks.len() && !is_word(&toks[i], "endbfchar") {
                        if let (Tok::Hex(code), Some(Tok::Hex(dst))) = (&toks[i], toks.get(i + 1)) {
                            cmap.single.insert(number_be(code), utf16be_to_string(dst));
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                }
                Tok::Word(w) if w == "beginbfrange" => {
                    i += 1;
                    i = parse_bfrange(&toks, i, &mut cmap);
                }
                _ => i += 1,
            }
        }
        cmap
    }
}

/// Parse the body of a `beginbfrange ... endbfrange` block starting at `i`.
/// Returns the index just after `endbfrange`.
fn parse_bfrange(toks: &[Tok], mut i: usize, cmap: &mut CMap) -> usize {
    while i < toks.len() && !is_word(&toks[i], "endbfrange") {
        let lo = match toks.get(i) {
            Some(Tok::Hex(b)) => b,
            _ => {
                i += 1;
                continue;
            }
        };
        let hi = match toks.get(i + 1) {
            Some(Tok::Hex(b)) => b,
            _ => {
                i += 1;
                continue;
            }
        };
        let begin = number_be(lo);
        let end = number_be(hi);
        match toks.get(i + 2) {
            // `<lo> <hi> [ <u0> <u1> ... ]` — one destination per code.
            Some(Tok::ArrOpen) => {
                let mut j = i + 3;
                let mut code = begin;
                while j < toks.len() && !matches!(toks[j], Tok::ArrClose) {
                    if let Tok::Hex(d) = &toks[j] {
                        cmap.single.insert(code, utf16be_to_string(d));
                        code += 1;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            // `<lo> <hi> <ustart>` — consecutive destinations.
            Some(Tok::Hex(d)) => {
                cmap.ranges.push(UnicodeInterval {
                    begin,
                    end,
                    start: number_be(d),
                    width: d.len(),
                });
                i += 3;
            }
            _ => i += 2,
        }
    }
    i + 1
}

// ---- tokenizer ----------------------------------------------------------

#[derive(Debug)]
enum Tok {
    Hex(Vec<u8>),
    ArrOpen,
    ArrClose,
    Word(String),
}

fn is_word(t: &Tok, w: &str) -> bool {
    matches!(t, Tok::Word(s) if s == w)
}

fn tokenize(data: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let c = data[i];
        match c {
            b'%' => {
                while i < data.len() && data[i] != b'\n' {
                    i += 1;
                }
            }
            b'<' => {
                if data.get(i + 1) == Some(&b'<') {
                    i += 2; // dict open — irrelevant here
                } else {
                    i += 1;
                    let mut hex = String::new();
                    while i < data.len() && data[i] != b'>' {
                        if !data[i].is_ascii_whitespace() {
                            hex.push(data[i] as char);
                        }
                        i += 1;
                    }
                    i += 1; // skip '>'
                    toks.push(Tok::Hex(hex_to_bytes(&hex)));
                }
            }
            b'>' => {
                i += if data.get(i + 1) == Some(&b'>') { 2 } else { 1 };
            }
            b'[' => {
                toks.push(Tok::ArrOpen);
                i += 1;
            }
            b']' => {
                toks.push(Tok::ArrClose);
                i += 1;
            }
            b'(' => {
                // literal string — skip balanced parens
                let mut depth = 1;
                i += 1;
                while i < data.len() && depth > 0 {
                    match data[i] {
                        b'\\' => i += 1,
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < data.len()
                    && !data[i].is_ascii_whitespace()
                    && !matches!(data[i], b'<' | b'>' | b'[' | b']' | b'(' | b'%')
                {
                    i += 1;
                }
                if i > start {
                    toks.push(Tok::Word(
                        String::from_utf8_lossy(&data[start..i]).into_owned(),
                    ));
                } else {
                    i += 1;
                }
            }
        }
    }
    toks
}

// ---- helpers ------------------------------------------------------------

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    let mut h = hex.to_string();
    if h.len() % 2 == 1 {
        h.push('0'); // PDF pads a trailing nibble with 0
    }
    (0..h.len())
        .step_by(2)
        .filter_map(|k| u8::from_str_radix(&h[k..k + 2], 16).ok())
        .collect()
}

/// Big-endian byte value (veraPDF `CMapParser.numberFromBytes`).
fn number_be(bytes: &[u8]) -> u32 {
    let mut r = 0u32;
    for &b in bytes {
        r = (r << 8) | b as u32;
    }
    r
}

/// Decode UTF-16BE bytes (the encoding of ToUnicode destinations). A single
/// byte is treated as Latin-1.
fn utf16be_to_string(bytes: &[u8]) -> String {
    if bytes.len() == 1 {
        return (bytes[0] as char).to_string();
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| {
            if c.len() == 2 {
                ((c[0] as u16) << 8) | c[1] as u16
            } else {
                c[0] as u16
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bfchar_and_codespace() {
        let cmap_text = b"\
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
2 beginbfchar
<0003> <0041>
<0006> <0061>
endbfchar
endcmap";
        let cm = CMap::parse(cmap_text);
        assert_eq!(cm.codespaces.len(), 1);
        // two-byte codespace → next_code consumes 2 bytes
        let (code, len) = cm.next_code(&[0x00, 0x03], 0);
        assert_eq!((code, len), (3, 2));
        assert_eq!(cm.unicode(3).as_deref(), Some("A"));
        assert_eq!(cm.unicode(6).as_deref(), Some("a"));
    }

    #[test]
    fn parses_bfrange_consecutive() {
        let cmap_text = b"\
1 beginbfrange
<0010> <0012> <0041>
endbfrange";
        let cm = CMap::parse(cmap_text);
        assert_eq!(cm.unicode(0x10).as_deref(), Some("A"));
        assert_eq!(cm.unicode(0x11).as_deref(), Some("B"));
        assert_eq!(cm.unicode(0x12).as_deref(), Some("C"));
    }
}
