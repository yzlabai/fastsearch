//! Text-file reading with encoding detection for the plain-text backends
//! (CSV/SRT/TeX): UTF-8 first, then `chardetng` detection + `encoding_rs`
//! decode. Legacy corpora are full of Shift-JIS/GBK/Windows-1252 files that
//! a bare `read_to_string` refuses (the G7 stress run surfaced a Shift-JIS
//! CSV); refusing real content over its encoding is data loss by another
//! name. Decoding is lossy on malformed sequences (U+FFFD), which is
//! visible in the output rather than silent.

use std::path::Path;

/// Read a text file: valid UTF-8 passes through byte-exact; anything else is
/// decoded via detection. Only I/O errors fail.
pub fn read_text(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(decode_text(&bytes))
}

/// Decode raw bytes to text (UTF-8 fast path, else detect + decode).
pub fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let mut det = chardetng::EncodingDetector::new();
            det.feed(bytes, true);
            let enc = det.guess(None, true);
            let (text, _, _) = enc.decode(bytes);
            text.into_owned()
        }
    }
}

/// Decode HTML bytes, honoring an in-document `<meta charset>` (sniffed in the
/// first 1024 bytes, per the HTML5 encoding-sniffing algorithm) before falling
/// back to UTF-8 / `chardetng`. A declared charset beats statistical detection
/// — a Windows-1252/GBK/Shift-JIS page that states its encoding decodes
/// exactly instead of being guessed (H7).
pub fn decode_html(bytes: &[u8]) -> String {
    if let Some(enc) = sniff_meta_charset(bytes) {
        if enc == encoding_rs::UTF_8 {
            if let Ok(s) = std::str::from_utf8(bytes) {
                return s.to_string();
            }
        }
        let (text, _, _) = enc.decode(bytes);
        return text.into_owned();
    }
    decode_text(bytes)
}

/// Find the first `charset` label in the document prefix and resolve it to an
/// encoding. Scans bytes (not yet decoded) for `charset` followed by an
/// optional `=`/quote/`:` run and the label token.
fn sniff_meta_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let head = &bytes[..bytes.len().min(1024)];
    let lower: Vec<u8> = head.iter().map(u8::to_ascii_lowercase).collect();
    let mut from = 0;
    while let Some(rel) = lower[from..].windows(7).position(|w| w == b"charset") {
        let i = from + rel + 7;
        let label: Vec<u8> = head[i..]
            .iter()
            .copied()
            .skip_while(|&b| matches!(b, b'=' | b' ' | b'"' | b'\'' | b':'))
            .take_while(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            .collect();
        if let Some(enc) = encoding_rs::Encoding::for_label(&label) {
            return Some(enc);
        }
        from = i;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through() {
        assert_eq!(decode_text("héllo 你好".as_bytes()), "héllo 你好");
    }

    #[test]
    fn shift_jis_detected() {
        // "名前" (name) in Shift-JIS.
        let sjis = [0x96u8, 0xBC, 0x91, 0x4F];
        assert_eq!(decode_text(&sjis), "名前");
    }

    #[test]
    fn gbk_detected() {
        // "中文测试" in GBK — long enough for the detector to commit.
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4, 0xB2, 0xE2, 0xCA, 0xD4];
        assert_eq!(decode_text(&gbk), "中文测试");
    }

    #[test]
    fn html_meta_charset_decodes_declared_encoding() {
        // A short GBK body that chardetng alone would likely misguess, but the
        // declared <meta charset> resolves exactly.
        let mut html = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        html.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4]); // 中文 in GBK
        html.extend_from_slice(b"</body></html>");
        assert!(decode_html(&html).contains("中文"));
    }

    #[test]
    fn html_meta_charset_http_equiv_form() {
        let mut html =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\">"
                .to_vec();
        html.push(0xE9); // é in Windows-1252
        assert!(decode_html(&html).contains('é'));
    }

    #[test]
    fn html_utf8_without_declaration() {
        // No meta → UTF-8 fast path / detection still works.
        assert!(decode_html("café 你好".as_bytes()).contains("café 你好"));
    }
}
