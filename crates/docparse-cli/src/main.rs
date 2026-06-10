//! `docparse` — parse a document into JSON / Markdown / text.

mod mcp;
mod server;

use clap::{Parser, Subcommand, ValueEnum};
use docparse_core::output;
use docparse_core::parser::DocumentParser;
use docparse_docx::DocxParser;
use docparse_html::HtmlParser;
use docparse_pdf::PdfParser;
use std::path::PathBuf;

/// Parser registry — one line per format backend. Shared by the CLI path, the
/// MCP server, and the REST server.
pub(crate) fn parsers() -> Vec<Box<dyn DocumentParser>> {
    vec![
        Box::new(PdfParser),
        Box::new(DocxParser),
        Box::new(HtmlParser),
    ]
}

/// Pick the backend by path and parse — the shared entry for all interfaces.
pub(crate) fn parse_path(path: &std::path::Path) -> anyhow::Result<docparse_core::ir::Document> {
    let parser = parsers()
        .into_iter()
        .find(|p| p.supports(path))
        .ok_or_else(|| anyhow::anyhow!("no parser supports {}", path.display()))?;
    parser.parse(path)
}

#[derive(Parser)]
#[command(
    name = "docparse",
    version,
    about = "Efficient multi-format document parser (Rust)"
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Input document (PDF, DOCX, or HTML).
    input: Option<PathBuf>,

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

    /// OCR quality-flagged pages (scans) with the embedded ONNX enhancer
    /// (PP-OCRv4 via tract). Digital pages never touch the model. Requires
    /// model files — see --ocr-models.
    #[arg(long)]
    ocr: bool,

    /// Directory holding ch_PP-OCRv4_{det,rec}_infer.onnx + ppocr_keys_v1.txt.
    #[arg(long, default_value = "models/ppocr")]
    ocr_models: PathBuf,

    /// Print the per-page complexity profile (kind/image-coverage/tables) as
    /// JSON to stderr — the routing signal, observable.
    #[arg(long)]
    profile: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the parser over MCP (newline-delimited JSON-RPC on stdio) so
    /// agents can call parse/chunk/locate directly.
    Mcp {
        /// Model dir for the optional `ocr: true` tool argument.
        #[arg(long, default_value = "models/ppocr")]
        ocr_models: PathBuf,
    },
    /// Serve a REST API on 127.0.0.1: POST /parse (multipart) + GET /healthz.
    Serve {
        /// TCP port to listen on.
        #[arg(long, default_value_t = 8642)]
        port: u16,
        /// Model dir for the optional `?ocr=true` query parameter.
        #[arg(long, default_value = "models/ppocr")]
        ocr_models: PathBuf,
    },
}

/// Lazily-loaded OCR enhancer shared by the serving faces: models are read on
/// the first request that asks for OCR, never at startup, so serving digital
/// documents stays model-free. The load outcome (ok or a stable error string)
/// is cached — broken setups fail fast on every call instead of re-reading.
pub(crate) struct OcrState {
    dir: PathBuf,
    cell: std::sync::OnceLock<Result<docparse_ocr::PpOcrEnhancer, String>>,
}

impl OcrState {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cell: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn get(&self) -> Result<&docparse_ocr::PpOcrEnhancer, String> {
        self.cell
            .get_or_init(|| {
                docparse_ocr::PpOcrEnhancer::new(&self.dir).map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// Run quality-routed enhancement over a parsed document (shared by all faces).
pub(crate) fn apply_ocr(
    doc: docparse_core::ir::Document,
    ocr: &docparse_ocr::PpOcrEnhancer,
) -> docparse_core::ir::Document {
    docparse_core::enhance::apply(&doc, &[ocr as &dyn docparse_core::enhance::Enhancer]).0
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

    match cli.command {
        Some(Command::Mcp { ocr_models }) => return mcp::serve(OcrState::new(ocr_models)),
        Some(Command::Serve { port, ocr_models }) => {
            return server::serve(port, OcrState::new(ocr_models))
        }
        None => {}
    }
    let input = cli
        .input
        .ok_or_else(|| anyhow::anyhow!("missing input file (see --help)"))?;

    let mut doc = parse_path(&input)?;

    if cli.ocr {
        // Load models only when some page actually needs enhancement — a
        // fully digital document with --ocr must stay zero-cost (and must not
        // fail on a missing model dir it would never use).
        let needs = docparse_core::quality::assess_pages(&doc)
            .iter()
            .any(|a| a.needs_enhancement);
        if needs {
            let ocr = docparse_ocr::PpOcrEnhancer::new(&cli.ocr_models)?;
            let (enhanced, report) = docparse_core::enhance::apply(&doc, &[&ocr]);
            doc = enhanced;
            eprintln!("{}", docparse_core::enhance::report_json(&report));
        } else {
            eprintln!("[]");
        }
    }

    if cli.quality {
        eprintln!("{}", docparse_core::quality::analyze(&doc).to_json());
    }
    if cli.profile {
        eprintln!(
            "{}",
            docparse_core::quality::profile_json(&docparse_core::quality::profile(&doc))
        );
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
        Format::Chunks => {
            docparse_core::chunk::to_json(&docparse_core::chunk::chunk_document(&doc))
        }
    };

    match cli.out {
        Some(path) => std::fs::write(path, rendered)?,
        None => println!("{rendered}"),
    }
    Ok(())
}
