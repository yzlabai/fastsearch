//! `docparse` — parse a document into JSON / Markdown / text.

use clap::{Parser, ValueEnum};
use docparse_core::output;
use docparse_core::parser::DocumentParser;
use docparse_docx::DocxParser;
use docparse_html::HtmlParser;
use docparse_pdf::PdfParser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "docparse", version, about = "Efficient multi-format document parser (Rust)")]
struct Cli {
    /// Input document (PDF, DOCX, or HTML).
    input: PathBuf,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Json)]
    format: Format,

    /// Write to this file instead of stdout.
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Print a parse-quality report (coverage/garble/flags) as JSON to stderr.
    #[arg(long)]
    quality: bool,

    /// Print the per-page enhancement routing plan (which pages a model would
    /// be escalated to) as JSON to stderr — demonstrates how few pages are hard.
    #[arg(long)]
    route_plan: bool,
}

#[derive(Clone, ValueEnum)]
enum Format {
    Json,
    Markdown,
    Text,
    /// Retrieval chunks with source page+bbox and heading breadcrumb (JSON).
    Chunks,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Parser registry — one line per format backend.
    let parsers: Vec<Box<dyn DocumentParser>> =
        vec![Box::new(PdfParser), Box::new(DocxParser), Box::new(HtmlParser)];
    let parser = parsers
        .into_iter()
        .find(|p| p.supports(&cli.input))
        .ok_or_else(|| anyhow::anyhow!("no parser supports {}", cli.input.display()))?;

    let doc = parser.parse(&cli.input)?;

    if cli.quality {
        eprintln!("{}", docparse_core::quality::analyze(&doc).to_json());
    }
    if cli.route_plan {
        // No enhancers registered in the CLI; the plan shows which pages WOULD
        // need a model — on a digital document this is empty (cost stays low).
        let plan = docparse_core::enhance::plan(&doc, &[]);
        eprintln!(
            "{{\"hard_pages\": {}, \"total_pages\": {}, \"routes\": {}}}",
            plan.len(),
            doc.pages.len(),
            docparse_core::enhance::report_json(&plan)
        );
    }

    let rendered = match cli.format {
        Format::Json => output::to_json(&doc)?,
        Format::Markdown => output::to_markdown(&doc),
        Format::Text => output::to_text(&doc),
        Format::Chunks => docparse_core::chunk::to_json(&docparse_core::chunk::chunk_document(&doc)),
    };

    match cli.out {
        Some(path) => std::fs::write(path, rendered)?,
        None => println!("{rendered}"),
    }
    Ok(())
}
