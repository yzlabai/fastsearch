//! Coverage-guided fuzz of the full PDF pipeline (G7): lopdf object model →
//! content-stream interpreter → table detectors. Contract: any byte salad
//! may error, never panic/hang.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = docparse_pdf::PdfParser::default().parse_bytes(data);
});
