//! Resource guards (roadmap module 9, plan N5b): cheap, format-agnostic caps
//! that stop a maliciously crafted document from exhausting memory or CPU
//! before any heavy work runs.
//!
//! These are *guards*, not normal limits — the thresholds sit far above any
//! legitimate document so real files are never rejected, but a zip bomb or a
//! pathological page count is refused with a traceable error (never a panic or
//! a hang). The pre-checks are pure metadata reads (ZIP central directory,
//! page count), not full decompression/parse.

/// Maximum pages a document may declare before we refuse it. A genuine book
/// runs hundreds of pages; tens of thousands signals a crafted object tree.
pub const MAX_PAGES: usize = 50_000;

/// Maximum total *declared* uncompressed size across a container's entries
/// (e.g. a DOCX zip). Caps absolute memory blow-up. 2 GiB clears any real
/// office document while stopping a bomb's terabyte expansion.
pub const MAX_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum overall compression ratio (declared uncompressed ÷ compressed). A
/// text-heavy DOCX compresses ~5-10×, image-heavy ~1-3×; classic zip bombs run
/// 10^3–10^11×. 250 is a wide safety margin that still catches bombs.
pub const MAX_COMPRESSION_RATIO: u64 = 250;

/// A resource-guard rejection. Carries enough to log/trace why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    /// More pages declared than [`MAX_PAGES`].
    TooManyPages { found: usize, limit: usize },
    /// Declared uncompressed total exceeds [`MAX_UNCOMPRESSED_BYTES`].
    UncompressedTooLarge { found: u64, limit: u64 },
    /// Compression ratio exceeds [`MAX_COMPRESSION_RATIO`] (zip-bomb shape).
    CompressionRatioTooHigh { ratio: u64, limit: u64 },
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitError::TooManyPages { found, limit } => {
                write!(f, "document declares {found} pages, over the {limit} guard")
            }
            LimitError::UncompressedTooLarge { found, limit } => write!(
                f,
                "declared uncompressed size {found} bytes exceeds the {limit} guard (possible zip bomb)"
            ),
            LimitError::CompressionRatioTooHigh { ratio, limit } => write!(
                f,
                "compression ratio {ratio}x exceeds the {limit}x guard (possible zip bomb)"
            ),
        }
    }
}

impl std::error::Error for LimitError {}

/// Guard a page count before per-page work begins.
pub fn check_page_count(found: usize) -> Result<(), LimitError> {
    if found > MAX_PAGES {
        Err(LimitError::TooManyPages {
            found,
            limit: MAX_PAGES,
        })
    } else {
        Ok(())
    }
}

/// Pre-check a ZIP container (DOCX/PPTX/XLSX) for bomb shape by reading only
/// the central directory — no entry is decompressed. Sums the *declared*
/// compressed and uncompressed sizes and applies the absolute + ratio guards.
///
/// On a buffer that isn't a parseable ZIP this returns `Ok(())`: it's a guard,
/// not a validator — the real parser will surface a genuine format error.
pub fn check_zip_bomb(bytes: &[u8]) -> Result<(), LimitError> {
    let Some((mut offset, entries)) = find_central_directory(bytes) else {
        return Ok(());
    };
    let mut total_uncompressed: u64 = 0;
    let mut total_compressed: u64 = 0;
    // Central directory file header signature.
    const CDFH: u32 = 0x0201_4b50;
    for _ in 0..entries {
        if offset + 46 > bytes.len() || read_u32(bytes, offset) != CDFH {
            break; // truncated/odd central dir — let the parser handle it
        }
        let compressed = read_u32(bytes, offset + 20) as u64;
        let uncompressed = read_u32(bytes, offset + 24) as u64;
        let name_len = read_u16(bytes, offset + 28) as usize;
        let extra_len = read_u16(bytes, offset + 30) as usize;
        let comment_len = read_u16(bytes, offset + 32) as usize;
        total_uncompressed = total_uncompressed.saturating_add(uncompressed);
        total_compressed = total_compressed.saturating_add(compressed);
        offset += 46 + name_len + extra_len + comment_len;
    }

    if total_uncompressed > MAX_UNCOMPRESSED_BYTES {
        return Err(LimitError::UncompressedTooLarge {
            found: total_uncompressed,
            limit: MAX_UNCOMPRESSED_BYTES,
        });
    }
    // Ratio only meaningful once there's non-trivial compressed payload.
    if let Some(ratio) = total_uncompressed.checked_div(total_compressed) {
        if ratio > MAX_COMPRESSION_RATIO {
            return Err(LimitError::CompressionRatioTooHigh {
                ratio,
                limit: MAX_COMPRESSION_RATIO,
            });
        }
    }
    Ok(())
}

/// Locate the ZIP central directory: scan back for the End Of Central Directory
/// record and return its (offset, entry-count). `None` if not a ZIP.
fn find_central_directory(bytes: &[u8]) -> Option<(usize, usize)> {
    const EOCD: u32 = 0x0605_4b50;
    if bytes.len() < 22 {
        return None;
    }
    // EOCD is 22 bytes + an optional comment (≤ 65535). Scan back over that window.
    let scan_start = bytes.len().saturating_sub(22 + 65_535);
    for i in (scan_start..=bytes.len() - 22).rev() {
        if read_u32(bytes, i) == EOCD {
            let entries = read_u16(bytes, i + 10) as usize;
            let cd_offset = read_u32(bytes, i + 16) as usize;
            // ZIP64 marks these 0xFFFF/0xFFFFFFFF; we don't parse ZIP64, so a
            // marker means "can't pre-check" → let the parser proceed.
            if cd_offset == 0xFFFF_FFFF || cd_offset >= bytes.len() {
                return None;
            }
            return Some((cd_offset, entries));
        }
    }
    None
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_count_guard() {
        assert!(check_page_count(1000).is_ok());
        assert_eq!(
            check_page_count(MAX_PAGES + 1),
            Err(LimitError::TooManyPages {
                found: MAX_PAGES + 1,
                limit: MAX_PAGES
            })
        );
    }

    #[test]
    fn non_zip_passes_through() {
        assert!(check_zip_bomb(b"not a zip file at all").is_ok());
        assert!(check_zip_bomb(&[]).is_ok());
    }

    /// Build a minimal ZIP (one stored entry) with forged central-directory
    /// sizes, to drive the guard without a real compressor.
    fn fake_zip(compressed: u32, uncompressed: u32) -> Vec<u8> {
        let mut z = Vec::new();
        // Local file header (content irrelevant to the central-dir guard).
        z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        z.extend_from_slice(&[0u8; 26]);
        let cd_offset = z.len() as u32;
        // Central directory file header.
        z.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // sig
        z.extend_from_slice(&[0u8; 16]); // version..crc (16 bytes to offset+20)
        z.extend_from_slice(&compressed.to_le_bytes()); // +20
        z.extend_from_slice(&uncompressed.to_le_bytes()); // +24
        z.extend_from_slice(&0u16.to_le_bytes()); // name len  +28
        z.extend_from_slice(&0u16.to_le_bytes()); // extra len +30
        z.extend_from_slice(&0u16.to_le_bytes()); // comment   +32
        z.extend_from_slice(&[0u8; 12]); // rest of 46-byte header
                                         // End of central directory.
        z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        z.extend_from_slice(&[0u8; 6]); // disk numbers + this-disk entries
        z.extend_from_slice(&1u16.to_le_bytes()); // total entries +10
        z.extend_from_slice(&46u32.to_le_bytes()); // cd size +12
        z.extend_from_slice(&cd_offset.to_le_bytes()); // cd offset +16
        z.extend_from_slice(&0u16.to_le_bytes()); // comment len +20
        z
    }

    #[test]
    fn realistic_ratio_passes() {
        // 1 MB → 8 MB (8x) is an ordinary text document.
        assert!(check_zip_bomb(&fake_zip(1_000_000, 8_000_000)).is_ok());
    }

    #[test]
    fn bomb_ratio_is_rejected() {
        // 1 KB → 1 GB is ~10^6×.
        let err = check_zip_bomb(&fake_zip(1_000, 1_000_000_000)).unwrap_err();
        assert!(matches!(err, LimitError::CompressionRatioTooHigh { .. }));
    }

    #[test]
    fn absolute_size_is_rejected() {
        // ~4 GiB stored 1:1 (ratio passes) trips the absolute cap. u32::MAX is
        // the largest a non-ZIP64 header can declare and already exceeds 2 GiB.
        let err = check_zip_bomb(&fake_zip(u32::MAX, u32::MAX)).unwrap_err();
        assert!(matches!(err, LimitError::UncompressedTooLarge { .. }));
    }
}
