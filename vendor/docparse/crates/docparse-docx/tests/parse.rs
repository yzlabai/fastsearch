//! Integration test for the DOCX backend against a small committed fixture
//! (headings + paragraphs + a 3×2 table), built with python-docx.

use docparse_core::ir::Element;

#[test]
fn parses_headings_paragraphs_and_table() {
    let bytes = include_bytes!("fixtures/sample.docx");
    let doc = docparse_docx::parse_bytes(bytes).expect("parse docx");

    let mut texts = Vec::new();
    let mut tables = 0;
    for el in doc.pages.iter().flat_map(|p| &p.elements) {
        match el {
            Element::Text(t) => texts.push(t.text.clone()),
            Element::Table(t) => {
                tables += 1;
                // Header row and a data cell came through.
                assert_eq!(t.rows[0][0].text, "Year");
                assert_eq!(t.rows[2][1].text, "1,450,000");
            }
            _ => {}
        }
    }

    assert!(
        texts.iter().any(|t| t == "Annual Report"),
        "heading text present"
    );
    assert!(
        texts.iter().any(|t| t.contains("introductory paragraph")),
        "body paragraph present"
    );
    assert_eq!(tables, 1, "one table");

    // Provenance is set.
    let prov = doc.provenance.expect("provenance");
    assert_eq!(prov.parser, "docx");
}

#[test]
fn rejects_zip_bomb_without_decompressing() {
    // A ZIP whose central directory forges a tiny compressed entry as a huge
    // uncompressed one (bomb shape). The guard reads only the central
    // directory, so the entry need not be a real deflate stream — it must be
    // refused before docx-rs decompresses anything (no hang).
    let (compressed, uncompressed): (u32, u32) = (1_000, 1_000_000_000); // ~10^6x
    let mut z: Vec<u8> = Vec::new();
    // Local file header — content irrelevant to the central-directory guard.
    z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    z.extend_from_slice(&[0u8; 26]);
    let cd_offset = z.len() as u32;
    // Central directory file header (sig + 16 bytes to reach the size fields).
    z.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    z.extend_from_slice(&[0u8; 16]);
    z.extend_from_slice(&compressed.to_le_bytes()); // +20
    z.extend_from_slice(&uncompressed.to_le_bytes()); // +24
    z.extend_from_slice(&0u16.to_le_bytes()); // name len  +28
    z.extend_from_slice(&0u16.to_le_bytes()); // extra len +30
    z.extend_from_slice(&0u16.to_le_bytes()); // comment   +32
    z.extend_from_slice(&[0u8; 12]); // rest of the 46-byte header
                                     // End of central directory.
    z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    z.extend_from_slice(&[0u8; 6]);
    z.extend_from_slice(&1u16.to_le_bytes()); // total entries +10
    z.extend_from_slice(&46u32.to_le_bytes()); // cd size +12
    z.extend_from_slice(&cd_offset.to_le_bytes()); // cd offset +16
    z.extend_from_slice(&0u16.to_le_bytes()); // comment len

    let err = docparse_docx::parse_bytes(&z).expect_err("bomb must be rejected");
    assert!(
        err.to_string().contains("zip bomb") || err.to_string().contains("ratio"),
        "expected a zip-bomb guard error, got: {err}"
    );
}
