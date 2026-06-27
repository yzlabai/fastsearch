//! Fuzz the EML backend (mail-parser + synthetic layout mapping).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = docparse_eml::parse_bytes(data);
});
