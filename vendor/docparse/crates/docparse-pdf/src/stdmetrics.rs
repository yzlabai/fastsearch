//! Standard-14 font metrics: glyph widths for the base fonts a PDF may use
//! *without* embedding a `/Widths` array (Helvetica, Times, Courier, Symbol,
//! ZapfDingbats and their bold/italic variants, plus common aliases like
//! Arial -> Helvetica).
//!
//! This is veraPDF's `StandardFontMetrics` / `AFMParser` / `StandardFontMetricsFactory`
//! equivalent: parse the Adobe Core-14 AFM files (`resources/afm/*.afm`, kept
//! verbatim with Adobe's notice in `MustRead.html`) into `glyph name -> width`
//! (1/1000 em). The AFM format is Adobe's; the parser here is independent.

use std::collections::HashMap;
use std::sync::OnceLock;

macro_rules! afm {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../resources/afm/", $name, ".afm")),
        )
    };
}

/// (canonical name, raw AFM text) for all 14 standard fonts.
const AFMS: &[(&str, &str)] = &[
    afm!("Helvetica"),
    afm!("Helvetica-Bold"),
    afm!("Helvetica-Oblique"),
    afm!("Helvetica-BoldOblique"),
    afm!("Times-Roman"),
    afm!("Times-Bold"),
    afm!("Times-Italic"),
    afm!("Times-BoldItalic"),
    afm!("Courier"),
    afm!("Courier-Bold"),
    afm!("Courier-Oblique"),
    afm!("Courier-BoldOblique"),
    afm!("Symbol"),
    afm!("ZapfDingbats"),
];

/// Parse one AFM's `C ... ; WX <w> ; N <name> ;` lines into name -> width.
fn parse_afm(raw: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        if !line.starts_with("C ") {
            continue;
        }
        let mut width = None;
        let mut name = None;
        for field in line.split(';') {
            let mut it = field.split_whitespace();
            match it.next() {
                Some("WX") => width = it.next().and_then(|v| v.parse::<f64>().ok()),
                Some("N") => name = it.next().map(|s| s.to_string()),
                _ => {}
            }
        }
        if let (Some(n), Some(w)) = (name, width) {
            map.insert(n, w);
        }
    }
    map
}

/// All 14 width tables, parsed once, keyed by canonical font name.
fn tables() -> &'static HashMap<&'static str, HashMap<String, f64>> {
    static T: OnceLock<HashMap<&'static str, HashMap<String, f64>>> = OnceLock::new();
    T.get_or_init(|| AFMS.iter().map(|(n, raw)| (*n, parse_afm(raw))).collect())
}

/// Resolve a PDF `/BaseFont` to one of the 14 canonical names, applying common
/// aliases (Arial->Helvetica, TimesNewRoman->Times, CourierNew->Courier) and
/// bold/italic style detection. Returns `None` if it is not a standard font.
fn canonical(base_font: &str) -> Option<&'static str> {
    // Strip subset prefix "ABCDEF+".
    let name = base_font
        .split_once('+')
        .map(|(_, n)| n)
        .unwrap_or(base_font);
    let lower = name.to_ascii_lowercase();

    let bold = lower.contains("bold");
    let italic = lower.contains("italic") || lower.contains("oblique");

    if lower.contains("zapfdingbats") || lower.contains("dingbats") {
        return Some("ZapfDingbats");
    }
    if lower.contains("symbol") {
        return Some("Symbol");
    }
    // Only recognize the standard-14 fonts and their well-known metric clones
    // (Arial≈Helvetica, Nimbus = Ghostscript's Core-14 substitutes). Being
    // stricter avoids applying standard widths to a genuinely different font
    // that merely lacks `/Widths`.
    let family = if lower.contains("courier") || lower.contains("nimbusmon") {
        Family::Courier
    } else if lower.contains("times") || lower.contains("nimbusrom") {
        Family::Times
    } else if lower.contains("helvetica") || lower.contains("arial") || lower.contains("nimbussan")
    {
        Family::Helvetica
    } else {
        return None;
    };

    Some(match (family, bold, italic) {
        (Family::Helvetica, false, false) => "Helvetica",
        (Family::Helvetica, true, false) => "Helvetica-Bold",
        (Family::Helvetica, false, true) => "Helvetica-Oblique",
        (Family::Helvetica, true, true) => "Helvetica-BoldOblique",
        (Family::Times, false, false) => "Times-Roman",
        (Family::Times, true, false) => "Times-Bold",
        (Family::Times, false, true) => "Times-Italic",
        (Family::Times, true, true) => "Times-BoldItalic",
        (Family::Courier, false, false) => "Courier",
        (Family::Courier, true, false) => "Courier-Bold",
        (Family::Courier, false, true) => "Courier-Oblique",
        (Family::Courier, true, true) => "Courier-BoldOblique",
    })
}

enum Family {
    Helvetica,
    Times,
    Courier,
}

/// Width table (glyph name -> 1/1000 em) for a standard font, or `None` if the
/// `/BaseFont` is not a recognized standard-14 font.
pub fn widths_for(base_font: &str) -> Option<&'static HashMap<String, f64>> {
    let key = canonical(base_font)?;
    tables().get(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helvetica_known_widths() {
        let w = widths_for("Helvetica").unwrap();
        assert_eq!(w.get("space"), Some(&278.0));
        assert_eq!(w.get("A"), Some(&667.0));
    }

    #[test]
    fn aliases_resolve() {
        assert!(widths_for("ABCDEF+Arial").is_some());
        assert!(widths_for("Arial-BoldMT").is_some());
        assert!(widths_for("TimesNewRomanPSMT").is_some());
        // Times stamp from a real sample.
        assert!(widths_for("Times-Roman").is_some());
        assert!(widths_for("NimbusRomNo9L-Regu").is_some());
    }

    #[test]
    fn style_detection() {
        assert_eq!(
            canonical("Arial-BoldItalicMT"),
            Some("Helvetica-BoldOblique")
        );
        assert_eq!(canonical("CourierNewPS-BoldMT"), Some("Courier-Bold"));
    }

    #[test]
    fn non_standard_returns_none() {
        assert!(widths_for("XYZShape+CMR10").is_none());
        assert!(widths_for("SomeRandomFont").is_none());
    }
}
