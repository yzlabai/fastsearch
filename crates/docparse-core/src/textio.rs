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
}
