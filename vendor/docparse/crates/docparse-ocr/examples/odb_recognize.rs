//! OmniDocBench evaluation helper: run UniRec directly on a cropped region
//! image (bypassing deterministic detection) to measure the model's raw
//! table/formula recognition against human ground truth. NOT shipped — an
//! eval-only tool mirroring OmniDocBench's single-module protocol.
//!
//! Input is a raw-RGB blob to avoid pulling an image decoder into the example:
//!   bytes 0..4   = width  (u32 LE)
//!   bytes 4..8   = height (u32 LE)
//!   bytes 8..    = width*height*3 RGB
//!
//! Usage: cargo run --release -p docparse-ocr --example odb_recognize -- \
//!          models/unirec crop.rgb [max_tokens]
//! Prints the recognized string (HTML for tables, LaTeX for formulas).

use docparse_ocr::unirec::UniRec;
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: odb_recognize <model_dir> <crop.rgb> [max_tokens]");
        std::process::exit(2);
    }
    let max_tokens = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2000);

    let blob = std::fs::read(&args[2])?;
    let w = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
    let h = u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize;
    let rgb = &blob[8..8 + w * h * 3];

    let model = UniRec::new(Path::new(&args[1]))?;
    let out = model.recognize(rgb, w, h, max_tokens)?;
    print!("{out}");
    Ok(())
}
