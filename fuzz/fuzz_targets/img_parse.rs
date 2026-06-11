//! Fuzz the image backend (zune-png / zune-jpeg headers + layout mapping).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = docparse_img::parse_bytes(data, "png");
    let _ = docparse_img::parse_bytes(data, "jpg");
});
