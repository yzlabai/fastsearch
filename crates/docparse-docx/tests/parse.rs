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

    assert!(texts.iter().any(|t| t == "Annual Report"), "heading text present");
    assert!(
        texts.iter().any(|t| t.contains("introductory paragraph")),
        "body paragraph present"
    );
    assert_eq!(tables, 1, "one table");

    // Provenance is set.
    let prov = doc.provenance.expect("provenance");
    assert_eq!(prov.parser, "docx");
}
