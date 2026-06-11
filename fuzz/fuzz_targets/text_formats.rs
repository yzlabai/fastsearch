//! Fuzz the text-format family: encoding detection, then the hand-rolled
//! SRT/VTT, LaTeX and CSV parsers on the decoded text.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = docparse_core::textio::decode_text(data);
    let _ = docparse_srt::parse_str(&text);
    let _ = docparse_tex::parse_str(&text);
    let _ = docparse_csv::parse_str(&text);
});
