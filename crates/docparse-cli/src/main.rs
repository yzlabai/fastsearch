//! `docparse` — parse a document into JSON / Markdown / text.

use clap::{Parser, ValueEnum};
use docparse_core::output;
use docparse_core::parser::DocumentParser;
use docparse_pdf::PdfParser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "docparse", version, about = "Efficient multi-format document parser (Rust)")]
struct Cli {
    /// Input document (currently supported: PDF).
    input: PathBuf,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Json)]
    format: Format,

    /// Write to this file instead of stdout.
    #[arg(short, long)]
    out: Option<PathBuf>,
}

#[derive(Clone, ValueEnum)]
enum Format {
    Json,
    Markdown,
    Text,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Parser registry — add DOCX/HTML backends here as they land.
    let parsers: Vec<Box<dyn DocumentParser>> = vec![Box::new(PdfParser)];
    let parser = parsers
        .into_iter()
        .find(|p| p.supports(&cli.input))
        .ok_or_else(|| anyhow::anyhow!("no parser supports {}", cli.input.display()))?;

    let doc = parser.parse(&cli.input)?;

    let rendered = match cli.format {
        Format::Json => output::to_json(&doc)?,
        Format::Markdown => output::to_markdown(&doc),
        Format::Text => output::to_text(&doc),
    };

    match cli.out {
        Some(path) => std::fs::write(path, rendered)?,
        None => println!("{rendered}"),
    }
    Ok(())
}
