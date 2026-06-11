//! `docparse` — parse a document into JSON / Markdown / text.

mod mcp;
mod server;

use clap::{Parser, Subcommand, ValueEnum};
use docparse_core::output;
use docparse_core::parser::DocumentParser;
use docparse_csv::CsvParser;
use docparse_docx::DocxParser;
use docparse_eml::EmlParser;
use docparse_html::HtmlParser;
use docparse_img::ImageParser;
use docparse_md::MarkdownParser;
use docparse_pdf::PdfParser;
use docparse_pptx::PptxParser;
use docparse_srt::SrtParser;
use docparse_tex::TexParser;
use docparse_xlsx::XlsxParser;
use std::path::PathBuf;

/// Parser registry — one line per format backend. Shared by the CLI path, the
/// MCP server, and the REST server. `decode_images` makes the PDF backend
/// materialize every embedded image's pixels (the image-export path).
pub(crate) fn parsers_with(decode_images: bool) -> Vec<Box<dyn DocumentParser>> {
    vec![
        Box::new(PdfParser { decode_images }),
        Box::new(DocxParser),
        Box::new(HtmlParser),
        Box::new(XlsxParser),
        Box::new(PptxParser),
        Box::new(MarkdownParser),
        Box::new(CsvParser),
        Box::new(SrtParser),
        Box::new(TexParser),
        Box::new(EmlParser),
        Box::new(ImageParser),
    ]
}

/// Pick the backend by path and parse — the shared entry for all interfaces.
pub(crate) fn parse_path_with(
    path: &std::path::Path,
    decode_images: bool,
) -> anyhow::Result<docparse_core::ir::Document> {
    let parser = parsers_with(decode_images)
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

    /// Re-derive macro reading order with the layout model (renders each page
    /// on demand — pure Rust, opt-in; PDF only). Heavier: ~2.4s/page.
    #[arg(long)]
    layout: bool,

    /// Path to the DocLayout-YOLO ONNX model.
    #[arg(long, default_value = "models/layout/doclayout_yolo.onnx")]
    layout_model: PathBuf,

    /// Caption sizable figures with a VLM (renders figure regions on demand;
    /// PDF only). Requires --vlm-url and --vlm-model. Captions are injected
    /// as positioned text with source "vlm:<model>".
    #[arg(long)]
    vlm_describe: bool,

    /// Re-extract detected tables' structure with a VLM (renders each table
    /// region on demand; PDF only). Handles merged cells / multi-row headers
    /// the geometric detectors can't. Requires --vlm-url and --vlm-model;
    /// replaced tables carry source "vlm:<model>", failures keep the
    /// deterministic grid.
    #[arg(long)]
    vlm_tables: bool,

    /// OpenAI-compatible service base URL (vLLM / Ollama / LM Studio / cloud),
    /// e.g. http://127.0.0.1:11434
    #[arg(long)]
    vlm_url: Option<String>,

    /// Vision model name as the service knows it (e.g. qwen2.5vl, llava).
    #[arg(long)]
    vlm_model: Option<String>,

    /// Bearer token, if the service requires one.
    #[arg(long)]
    vlm_api_key: Option<String>,

    /// Re-extract detected tables' structure with the embedded UniRec-0.1B
    /// model (renders each table region on demand; PDF only). Resolves
    /// merged cells / multi-row headers in-process — no service needed.
    /// Value: model dir holding encoder/decoder ONNX + tokenizer mapping.
    /// Replaced tables carry source "table:unirec-0.1b"; failures keep the
    /// deterministic grid.
    #[arg(long, value_name = "DIR")]
    table_model: Option<PathBuf>,

    /// Convert display formulas to LaTeX with the embedded UniRec-0.1B
    /// model (PDF only). Formula regions come from the DocLayout-YOLO
    /// layout model (--layout-model path); glyph-soup text inside each
    /// region is replaced by one LaTeX chunk tagged "Formula" with source
    /// "formula:unirec-0.1b". Value: UniRec model directory.
    #[arg(long, value_name = "DIR")]
    formula_model: Option<PathBuf>,

    /// Embed image payloads as base64 in JSON output (data_base64 +
    /// data_media_type on each image element) — ODL's "embedded" mode.
    /// Decodes all embedded images ≥16px a side (PDF) or the input image.
    #[arg(long)]
    image_embed: bool,

    /// Export embedded raster images (≥16px a side) to this directory as
    /// JPEG/PNG files; JSON image elements gain a "file" path and Markdown
    /// references them (PDF only). Mirrors ODL's external image output.
    #[arg(long)]
    image_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the parser over MCP (newline-delimited JSON-RPC on stdio) so
    /// agents can call parse/chunk/locate directly.
    Mcp {
        /// Model dir for the optional `ocr: true` tool argument.
        #[arg(long, default_value = "models/ppocr")]
        ocr_models: PathBuf,
        /// DocLayout-YOLO path for `layout`/`formula_model` tool arguments.
        #[arg(long, default_value = "models/layout/doclayout_yolo.onnx")]
        layout_model: PathBuf,
        /// UniRec model dir enabling `table_model`/`formula_model` arguments.
        #[arg(long)]
        unirec_models: Option<PathBuf>,
        /// OpenAI-compatible service URL enabling `vlm_describe`/`vlm_tables`.
        #[arg(long)]
        vlm_url: Option<String>,
        /// Vision model name for the VLM service.
        #[arg(long)]
        vlm_model: Option<String>,
        /// Bearer token for the VLM service.
        #[arg(long)]
        vlm_api_key: Option<String>,
    },
    /// Serve a REST API on 127.0.0.1: POST /parse (multipart) + GET /healthz.
    Serve {
        /// TCP port to listen on.
        #[arg(long, default_value_t = 8642)]
        port: u16,
        /// Model dir for the optional `?ocr=true` query parameter.
        #[arg(long, default_value = "models/ppocr")]
        ocr_models: PathBuf,
        /// DocLayout-YOLO path for `?layout=true` / `?formula_model=true`.
        #[arg(long, default_value = "models/layout/doclayout_yolo.onnx")]
        layout_model: PathBuf,
        /// UniRec model dir enabling `?table_model=true` / `?formula_model=true`.
        #[arg(long)]
        unirec_models: Option<PathBuf>,
        /// OpenAI-compatible service URL enabling `?vlm_describe=true` / `?vlm_tables=true`.
        #[arg(long)]
        vlm_url: Option<String>,
        /// Vision model name for the VLM service.
        #[arg(long)]
        vlm_model: Option<String>,
        /// Bearer token for the VLM service.
        #[arg(long)]
        vlm_api_key: Option<String>,
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

fn vlm_config(
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
) -> Option<docparse_vlm::VlmConfig> {
    match (url, model) {
        (Some(url), Some(model)) => Some(docparse_vlm::VlmConfig {
            url,
            model,
            api_key,
        }),
        _ => None,
    }
}

/// Per-request enhancement switches for the serving faces (MCP tool args /
/// REST query params). Everything defaults off — the deterministic result.
#[derive(Default, Clone, Copy)]
pub(crate) struct EnhanceOpts {
    pub ocr: bool,
    /// Embed image payloads as base64 in the JSON output (serving counterpart
    /// of --image-dir; ODL's image_output="embedded").
    pub images_embedded: bool,
    pub layout: bool,
    pub table_model: bool,
    pub formula_model: bool,
    pub vlm_describe: bool,
    pub vlm_tables: bool,
}

impl EnhanceOpts {
    fn any_pdf_only(&self) -> bool {
        self.layout
            || self.table_model
            || self.formula_model
            || self.vlm_describe
            || self.vlm_tables
    }
}

/// Server-lifetime enhancement state: capability config from startup flags +
/// lazily-loaded models shared across requests (UniRec is ~700MB — loading
/// once per server is the point). A capability whose config is absent yields
/// a clear per-request error naming the startup flag, never a crash.
pub(crate) struct EnhanceState {
    pub ocr: OcrState,
    layout_model: PathBuf,
    unirec_dir: Option<PathBuf>,
    vlm: Option<docparse_vlm::VlmConfig>,
    unirec: std::sync::OnceLock<Result<std::sync::Arc<docparse_ocr::unirec::UniRec>, String>>,
}

impl EnhanceState {
    pub(crate) fn new(
        ocr_models: PathBuf,
        layout_model: PathBuf,
        unirec_dir: Option<PathBuf>,
        vlm: Option<docparse_vlm::VlmConfig>,
    ) -> Self {
        Self {
            ocr: OcrState::new(ocr_models),
            layout_model,
            unirec_dir,
            vlm,
            unirec: std::sync::OnceLock::new(),
        }
    }

    fn unirec(&self) -> anyhow::Result<std::sync::Arc<docparse_ocr::unirec::UniRec>> {
        let dir = self.unirec_dir.as_ref().ok_or_else(|| {
            anyhow::anyhow!("table/formula model not configured (start with --unirec-models <dir>)")
        })?;
        self.unirec
            .get_or_init(|| {
                docparse_ocr::unirec::UniRec::new(dir)
                    .map(std::sync::Arc::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(|e| anyhow::anyhow!("unirec models unavailable: {e}"))
    }

    /// Apply the requested enhancements in the CLI's order. PDF-only
    /// enhancements are skipped for other formats (documented in the tool
    /// descriptions); unconfigured capabilities error with the startup flag
    /// to set.
    pub(crate) fn apply(
        &self,
        mut doc: docparse_core::ir::Document,
        path: &std::path::Path,
        o: EnhanceOpts,
    ) -> anyhow::Result<docparse_core::ir::Document> {
        if o.ocr {
            let enhancer = self
                .ocr
                .get()
                .map_err(|e| anyhow::anyhow!("ocr models unavailable: {e}"))?;
            doc = apply_ocr(doc, enhancer);
        }
        if o.images_embedded {
            embed_images(&mut doc);
        }
        let is_pdf = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !o.any_pdf_only() || !is_pdf {
            return Ok(doc);
        }
        if o.layout {
            let bytes = std::fs::read(path)?;
            docparse_ocr::layout::enhance_document(&mut doc, bytes, &self.layout_model, 2.0)?;
        }
        if o.table_model {
            let model = self.unirec()?;
            docparse_ocr::table_model::refine_tables(&mut doc, std::fs::read(path)?, &model)?;
        }
        if o.formula_model {
            let model = self.unirec()?;
            docparse_ocr::formula::enhance_formulas(
                &mut doc,
                std::fs::read(path)?,
                &self.layout_model,
                &model,
            )?;
        }
        if o.vlm_describe || o.vlm_tables {
            let cfg = self.vlm.clone().ok_or_else(|| {
                anyhow::anyhow!("vlm not configured (start with --vlm-url and --vlm-model)")
            })?;
            let client = docparse_vlm::VlmClient::new(cfg);
            if o.vlm_describe {
                docparse_vlm::annotate_pictures(&mut doc, std::fs::read(path)?, &client)?;
            }
            if o.vlm_tables {
                docparse_vlm::refine_tables(&mut doc, std::fs::read(path)?, &client)?;
            }
        }
        Ok(doc)
    }
}

/// Write each decoded image to `dir` (JPEG passthrough as-is; raw Gray8/Rgb8
/// bitmaps as PNG) and record the path on the element so JSON/Markdown can
/// reference it. Returns the number of files written. Position-only images
/// (unsupported encodings, below the size gate) are skipped — they keep their
/// bbox in JSON for audit, same as before.
fn export_images(
    doc: &mut docparse_core::ir::Document,
    dir: &std::path::Path,
) -> anyhow::Result<usize> {
    use docparse_core::ir::{Element, ImageKind};
    std::fs::create_dir_all(dir)?;
    let mut written = 0usize;
    for page in &mut doc.pages {
        let mut idx = 0usize;
        for el in &mut page.elements {
            let Element::Image(img) = el else { continue };
            if img.data.is_empty() {
                continue;
            }
            let (ext, bytes) = match img.kind {
                ImageKind::Jpeg => ("jpg", std::mem::take(&mut img.data)),
                ImageKind::Rgb8 => (
                    "png",
                    docparse_vlm::encode_png_rgb(&img.data, img.width_px, img.height_px),
                ),
                ImageKind::Gray8 => {
                    let rgb: Vec<u8> = img.data.iter().flat_map(|&g| [g, g, g]).collect();
                    (
                        "png",
                        docparse_vlm::encode_png_rgb(&rgb, img.width_px, img.height_px),
                    )
                }
                ImageKind::None => continue,
            };
            idx += 1;
            let name = format!("p{}-{}.{}", page.number, idx, ext);
            let path = dir.join(&name);
            std::fs::write(&path, bytes)?;
            img.file = Some(path.display().to_string());
            written += 1;
        }
    }
    Ok(written)
}

/// Fill `data_base64`/`data_media_type` on every image that carries pixels
/// (JPEG passthrough as-is; raw bitmaps re-encoded as PNG) — the embedded
/// counterpart of `--image-dir` (ODL `image_output="embedded"`). Returns the
/// number of images embedded.
pub(crate) fn embed_images(doc: &mut docparse_core::ir::Document) -> usize {
    use base64::Engine;
    use docparse_core::ir::{Element, ImageKind};
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut n = 0usize;
    for page in &mut doc.pages {
        for el in &mut page.elements {
            let Element::Image(img) = el else { continue };
            if img.data.is_empty() {
                continue;
            }
            let (mime, bytes) = match img.kind {
                ImageKind::Jpeg => ("image/jpeg", img.data.clone()),
                ImageKind::Rgb8 => (
                    "image/png",
                    docparse_vlm::encode_png_rgb(&img.data, img.width_px, img.height_px),
                ),
                ImageKind::Gray8 => {
                    let rgb: Vec<u8> = img.data.iter().flat_map(|&g| [g, g, g]).collect();
                    (
                        "image/png",
                        docparse_vlm::encode_png_rgb(&rgb, img.width_px, img.height_px),
                    )
                }
                ImageKind::None => continue,
            };
            img.data_base64 = Some(b64.encode(bytes));
            img.data_media_type = Some(mime.to_string());
            n += 1;
        }
    }
    n
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
        Some(Command::Mcp {
            ocr_models,
            layout_model,
            unirec_models,
            vlm_url,
            vlm_model,
            vlm_api_key,
        }) => {
            return mcp::serve(EnhanceState::new(
                ocr_models,
                layout_model,
                unirec_models,
                vlm_config(vlm_url, vlm_model, vlm_api_key),
            ))
        }
        Some(Command::Serve {
            port,
            ocr_models,
            layout_model,
            unirec_models,
            vlm_url,
            vlm_model,
            vlm_api_key,
        }) => {
            return server::serve(
                port,
                EnhanceState::new(
                    ocr_models,
                    layout_model,
                    unirec_models,
                    vlm_config(vlm_url, vlm_model, vlm_api_key),
                ),
            )
        }
        None => {}
    }
    let input = cli
        .input
        .ok_or_else(|| anyhow::anyhow!("missing input file (see --help)"))?;

    let mut doc = parse_path_with(&input, cli.image_dir.is_some() || cli.image_embed)?;

    if let Some(dir) = &cli.image_dir {
        let n = export_images(&mut doc, dir)?;
        eprintln!("{{\"images_exported\": {n}}}");
    }
    if cli.image_embed {
        let n = embed_images(&mut doc);
        eprintln!("{{\"images_embedded\": {n}}}");
    }

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

    if cli.layout {
        if input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false)
        {
            let pdf_bytes = std::fs::read(&input)?;
            let n = docparse_ocr::layout::enhance_document(
                &mut doc,
                pdf_bytes,
                &cli.layout_model,
                2.0,
            )?;
            eprintln!("{{\"layout_enhanced_pages\": {n}}}");
        } else {
            eprintln!("--layout currently supports PDF inputs only; skipped");
        }
    }

    if let Some(dir) = &cli.table_model {
        let is_pdf = input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !is_pdf {
            eprintln!("--table-model currently supports PDF inputs only; skipped");
        } else {
            let model = docparse_ocr::unirec::UniRec::new(dir)?;
            let n =
                docparse_ocr::table_model::refine_tables(&mut doc, std::fs::read(&input)?, &model)?;
            eprintln!("{{\"table_model_refined\": {n}}}");
        }
    }

    if let Some(dir) = &cli.formula_model {
        let is_pdf = input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !is_pdf {
            eprintln!("--formula-model currently supports PDF inputs only; skipped");
        } else {
            let model = docparse_ocr::unirec::UniRec::new(dir)?;
            let n = docparse_ocr::formula::enhance_formulas(
                &mut doc,
                std::fs::read(&input)?,
                &cli.layout_model,
                &model,
            )?;
            eprintln!("{{\"formula_model_replaced\": {n}}}");
        }
    }

    if cli.vlm_describe || cli.vlm_tables {
        let is_pdf = input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);
        if !is_pdf {
            eprintln!("--vlm-describe/--vlm-tables currently support PDF inputs only; skipped");
        } else {
            let (url, model) = match (cli.vlm_url.clone(), cli.vlm_model.clone()) {
                (Some(u), Some(m)) => (u, m),
                _ => anyhow::bail!("--vlm-describe/--vlm-tables require --vlm-url and --vlm-model"),
            };
            let client = docparse_vlm::VlmClient::new(docparse_vlm::VlmConfig {
                url,
                model,
                api_key: cli.vlm_api_key.clone(),
            });
            if cli.vlm_describe {
                let n = docparse_vlm::annotate_pictures(&mut doc, std::fs::read(&input)?, &client)?;
                eprintln!("{{\"vlm_described_figures\": {n}}}");
            }
            if cli.vlm_tables {
                let n = docparse_vlm::refine_tables(&mut doc, std::fs::read(&input)?, &client)?;
                eprintln!("{{\"vlm_refined_tables\": {n}}}");
            }
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
