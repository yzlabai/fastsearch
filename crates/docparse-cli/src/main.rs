//! `docparse` — parse a document into JSON / Markdown / text.

mod batch;
mod mcp;
mod progress;
mod resources;
mod server;

use clap::{Parser, Subcommand, ValueEnum};
use docparse_adoc::AdocParser;
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
        Box::new(AdocParser),
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

    /// Input document(s) and/or folder(s). One file → result to stdout (or
    /// -o). Multiple inputs, a folder, or --out-dir → batch mode: each input is
    /// parsed and an aggregate report is printed at the end.
    inputs: Vec<PathBuf>,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = Format::Json)]
    format: Format,

    /// Table rendering inside `-f chunks` text (tab=default, markdown=pipe table).
    #[arg(long, value_enum, default_value_t = TableFormat::Tab)]
    table_format: TableFormat,

    /// Write to this file instead of stdout. For `-f okf` this is the bundle
    /// *directory* (omit it to auto-derive `<stem>-okf/`).
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// `-f okf`: prefix for each concept's `resource` URI (e.g.
    /// `file:///data/docs/`). Default empty → the bare source basename, which
    /// keeps bundles byte-identical across machines.
    #[arg(long, value_name = "URI")]
    okf_resource_base: Option<String>,

    /// `-f okf`: overwrite the target bundle directory even if it exists and is
    /// non-empty (otherwise an auto-derived non-empty dir is refused).
    #[arg(long)]
    force: bool,

    /// `-f okf`: write the bundle as a deterministic tar archive to stdout
    /// (for `| tar x` / upload) instead of a directory. Ignores -o.
    #[arg(long)]
    okf_tar: bool,

    /// Print a parse-quality report (coverage/garble/flags) as JSON to stderr.
    #[arg(long)]
    quality: bool,

    /// Print the per-page enhancement routing plan (which pages a model would
    /// be escalated to) as JSON to stderr — demonstrates how few pages are hard.
    #[arg(long)]
    route_plan: bool,

    /// OCR quality-flagged pages (scans) with the embedded ONNX enhancer
    /// (PP-OCRv6 via tract). Digital pages never touch the model. Requires
    /// model files — see --ocr-models.
    #[arg(long)]
    ocr: bool,

    /// PP-OCR model dir (*det*.onnx / *rec*.onnx / *dict*.txt; any generation).
    /// Default models/ppocr-v6 (PP-OCRv6 tiny); pass models/ppocr for v4.
    #[arg(long, default_value = "models/ppocr-v6")]
    ocr_models: PathBuf,

    /// Print the per-page complexity profile (kind/image-coverage/tables) as
    /// JSON to stderr — the routing signal, observable.
    #[arg(long)]
    profile: bool,

    /// Re-derive macro reading order with the layout model (renders each page
    /// on demand — pure Rust, opt-in; PDF only). Heavier: ~2.4s/page.
    #[arg(long)]
    layout: bool,

    /// Path to the layout ONNX model. Backend is auto-detected: DocLayout-YOLO
    /// (default) or PP-DocLayoutV2 (pass models/layout-ppv2/PP-DoclayoutV2_simp.onnx
    /// — richer 25-class semantics + native reading order; ~3x YOLO on
    /// messy-layout table detection).
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

    /// OpenAI-compatible service base URL (vLLM / LM Studio / cloud),
    /// e.g. http://127.0.0.1:8000
    #[arg(long)]
    vlm_url: Option<String>,

    /// Vision model name as the service knows it.
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

    /// Re-recognize whole pages with the embedded UniRec model (PDF only):
    /// layout regions (DocLayout-YOLO) read in order, replacing the page's
    /// text at region-level positions. The route for design/CJK layouts the
    /// deterministic geometry can't order — opt-in, line-level positions are
    /// traded away (chunks carry region bboxes). Value: UniRec model dir.
    #[arg(long, value_name = "DIR")]
    transcribe_model: Option<PathBuf>,

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

    /// Progress & speed visualization on stderr: auto (interactive TTY only,
    /// the default), always (force, even when piped), never (off), json
    /// (machine-readable JSON-lines events for CI/wrappers — no bar/ANSI).
    /// Shows a per-phase spinner / page bar and an end-of-run pages/s · MB/s
    /// summary. Never touches stdout, so `-f json > out.json` stays clean.
    #[arg(long, value_enum, default_value_t = progress::ProgressMode::Auto)]
    progress: progress::ProgressMode,

    /// Silence progress visualization (alias for --progress never).
    #[arg(long)]
    quiet: bool,

    /// Print CPU & peak-memory usage for the run to stderr at the end: peak RSS,
    /// CPU time (user+sys), and average utilization (>100% = multi-core work).
    /// Under --progress json it's emitted as a "resources" event instead.
    #[arg(long)]
    stats: bool,

    /// Batch output directory: write one result file per input as
    /// <out-dir>/<stem>.<format-ext> (json/md/txt). Required to keep parsed
    /// content when processing more than one file. Created if missing.
    #[arg(long, value_name = "DIR")]
    out_dir: Option<PathBuf>,

    /// In batch mode, descend into sub-folders. Default: only the folder's top
    /// level. No effect on explicit file inputs.
    #[arg(short, long)]
    recursive: bool,

    /// In batch mode, process up to N files in parallel (default 1 = serial).
    /// Only applies to deterministic batches: when any model flag (--ocr,
    /// --layout, --table-model, --formula-model, --transcribe-model, --vlm-*)
    /// is set, jobs is forced to 1 to keep peak memory bounded (per-page scan
    /// buffers + ~700MB models would multiply across files). Capped at the
    /// core count.
    #[arg(long, value_name = "N", default_value_t = 1)]
    jobs: usize,

    /// In batch mode, also write the aggregate report as JSON to this file.
    #[arg(long, value_name = "FILE")]
    report_json: Option<PathBuf>,

    /// In batch mode, also write the aggregate report as CSV (one row per file)
    /// to this file.
    #[arg(long, value_name = "FILE")]
    report_csv: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the parser over MCP (newline-delimited JSON-RPC on stdio) so
    /// agents can call parse/chunk/locate directly.
    Mcp {
        /// Model dir for the optional `ocr: true` tool argument.
        #[arg(long, default_value = "models/ppocr-v6")]
        ocr_models: PathBuf,
        /// Layout ONNX path for `layout`/`formula_model` tool arguments
        /// (DocLayout-YOLO or PP-DocLayoutV2, auto-detected).
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
    /// Serve a REST API: POST /parse (multipart) + GET /healthz.
    Serve {
        /// Bind address. Default 127.0.0.1 (same-machine trust model); set
        /// 0.0.0.0 only behind a trusted network boundary (e.g. a container on
        /// a private compose network) — the API has no auth.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// TCP port to listen on.
        #[arg(long, default_value_t = 8642)]
        port: u16,
        /// Model dir for the optional `?ocr=true` query parameter.
        #[arg(long, default_value = "models/ppocr-v6")]
        ocr_models: PathBuf,
        /// Layout ONNX path for `?layout=true` / `?formula_model=true`
        /// (DocLayout-YOLO or PP-DocLayoutV2, auto-detected).
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
                ensure_ocr_models(&self.dir).map_err(|e| format!("{e:#}"))?;
                docparse_ocr::PpOcrEnhancer::new(&self.dir).map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// A UniRec model loaded on first use and cached for the rest of the run — the
/// CLI-path analogue of `EnhanceState`'s server-lifetime UniRec. The model is
/// ~700 MB to load, so a batch over many files (`--table-model` / `--formula-model`
/// / `--transcribe-model`) must read it once, not once per file.
pub(crate) struct LazyUniRec {
    cell: std::sync::OnceLock<Result<docparse_ocr::unirec::UniRec, String>>,
}

impl LazyUniRec {
    fn new() -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
        }
    }

    /// The model dir is fixed for a run (one CLI flag), so `dir` is the same on
    /// every call; the first load wins and is reused.
    fn get(&self, dir: &std::path::Path) -> anyhow::Result<&docparse_ocr::unirec::UniRec> {
        self.cell
            .get_or_init(|| docparse_ocr::unirec::UniRec::new(dir).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("unirec models unavailable: {e}"))
    }
}

/// A layout (DocLayout-YOLO / PP-DocLayoutV2) model loaded on first use and
/// cached — `--layout`/`--formula-model`/`--transcribe-model` all need it, so a
/// batch (or a single file using several of them) reads it once.
pub(crate) struct LazyLayout {
    cell: std::sync::OnceLock<Result<docparse_ocr::layout::LayoutModel, String>>,
}

impl LazyLayout {
    fn new() -> Self {
        Self {
            cell: std::sync::OnceLock::new(),
        }
    }

    fn get(&self, path: &std::path::Path) -> anyhow::Result<&docparse_ocr::layout::LayoutModel> {
        self.cell
            .get_or_init(|| {
                docparse_ocr::layout::LayoutModel::new(path).map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("layout model unavailable: {e}"))
    }
}

/// Models loaded once per CLI run and reused across every input. In single-file
/// mode this is just the one file; in batch mode it's the whole folder — so the
/// heavy OCR / UniRec / layout models are read at most once, not per file. All
/// fields are lazy (interior-mutable `OnceLock`): a digital-only `--ocr` batch
/// still never touches a model, preserving the "digital stays model-free"
/// invariant.
pub(crate) struct RunModels {
    ocr: OcrState,
    layout: LazyLayout,
    table: LazyUniRec,
    formula: LazyUniRec,
    transcribe: LazyUniRec,
}

impl RunModels {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            ocr: OcrState::new(cli.ocr_models.clone()),
            layout: LazyLayout::new(),
            table: LazyUniRec::new(),
            formula: LazyUniRec::new(),
            transcribe: LazyUniRec::new(),
        }
    }
}

/// Make sure the OCR model dir is populated before the enhancer reads it.
///
/// For the built-in PP-OCRv6 default we can fetch the ~7 MB model set on first
/// use. Downloading is a network action, so it's gated on an interactive y/N
/// confirm; non-interactive faces (MCP/REST servers, pipes, CI) aren't a TTY
/// and get a clear error with the fetch command instead. `DOCPARSE_OCR_DOWNLOAD=1`
/// pre-confirms for automation that explicitly opts in.
fn ensure_ocr_models(dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::{IsTerminal, Write};
    if docparse_ocr::fetch::models_present(dir) {
        return Ok(());
    }
    let fetch_cmd = "./scripts/fetch-models.sh ppocr-v6";
    if !docparse_ocr::fetch::is_default_v6_dir(dir) {
        anyhow::bail!(
            "OCR models not found in {}\n  download them with: {fetch_cmd}",
            dir.display()
        );
    }
    let preconfirmed = std::env::var_os("DOCPARSE_OCR_DOWNLOAD").is_some();
    if !preconfirmed {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "OCR models not found at {}\n  run: {fetch_cmd}\n  \
                 or set DOCPARSE_OCR_DOWNLOAD=1 to fetch non-interactively (~7 MB, Apache-2.0)",
                dir.display()
            );
        }
        eprint!(
            "OCR models missing. Download PP-OCRv6 tiny (~7 MB, PaddlePaddle, Apache-2.0) \
             to {}? [y/N] ",
            dir.display()
        );
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            anyhow::bail!("declined — fetch later with: {fetch_cmd}");
        }
    }
    docparse_ocr::fetch::fetch_ppocr_v6(dir, |name| eprintln!("  ↓ {name}"))
        .context("downloading PP-OCRv6 models")?;
    eprintln!("  ✓ OCR models ready at {}", dir.display());
    Ok(())
}

/// Run quality-routed enhancement over a parsed document (shared by all faces).
/// Remove empty-row table placeholders (a `--layout`-seeded table region no
/// model filled, or `--table-model` not run). Called after ALL enhancers so
/// every output face is consistent — including `-f json`, which serializes
/// `page.elements` directly and otherwise leaks `{"type":"table","rows":[]}`.
fn drop_empty_table_placeholders(doc: &mut docparse_core::ir::Document) {
    for page in &mut doc.pages {
        page.elements
            .retain(|e| !matches!(e, docparse_core::ir::Element::Table(t) if t.rows.is_empty()));
    }
}

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
    layout: std::sync::OnceLock<Result<std::sync::Arc<docparse_ocr::layout::LayoutModel>, String>>,
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
            layout: std::sync::OnceLock::new(),
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

    /// Layout model loaded once per server lifetime (lazy), shared across
    /// requests — the serving counterpart of the CLI's `RunModels.layout`.
    fn loaded_layout(&self) -> anyhow::Result<std::sync::Arc<docparse_ocr::layout::LayoutModel>> {
        self.layout
            .get_or_init(|| {
                docparse_ocr::layout::LayoutModel::new(&self.layout_model)
                    .map(std::sync::Arc::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(|e| anyhow::anyhow!("layout model unavailable: {e}"))
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
            let layout = self.loaded_layout()?;
            docparse_ocr::layout::enhance_document(&mut doc, bytes, &layout, 2.0)?;
        }
        if o.table_model {
            let model = self.unirec()?;
            docparse_ocr::table_model::refine_tables(&mut doc, std::fs::read(path)?, &model)?;
        }
        if o.formula_model {
            let layout = self.loaded_layout()?;
            let model = self.unirec()?;
            docparse_ocr::formula::enhance_formulas(
                &mut doc,
                std::fs::read(path)?,
                &layout,
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
        drop_empty_table_placeholders(&mut doc);
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

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Markdown,
    Text,
    /// Retrieval chunks with source page+bbox and heading breadcrumb (JSON).
    Chunks,
    /// Document structure tree: nested sections (title/level/page/bbox) for
    /// agentic navigation — list the table of contents, drill into a section (JSON).
    Outline,
    /// Open Knowledge Format bundle: a directory of Markdown + YAML-frontmatter
    /// "concept" files mirroring the structure tree (git-native, citable RAG
    /// delivery). Writes a directory (`-o <dir>`, else auto-derived `<stem>-okf/`).
    Okf,
}

/// Table cell rendering inside `chunks` text.
#[derive(Clone, Copy, ValueEnum)]
enum TableFormat {
    /// Tab/newline separated (default, compact).
    Tab,
    /// GitHub pipe table (markdown-native consumers).
    Markdown,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Borrow (not move) the subcommand so the whole `cli` stays available to the
    // file-processing path below — server fields are cheap to clone.
    if let Some(cmd) = &cli.command {
        match cmd {
            Command::Mcp {
                ocr_models,
                layout_model,
                unirec_models,
                vlm_url,
                vlm_model,
                vlm_api_key,
            } => {
                return mcp::serve(EnhanceState::new(
                    ocr_models.clone(),
                    layout_model.clone(),
                    unirec_models.clone(),
                    vlm_config(vlm_url.clone(), vlm_model.clone(), vlm_api_key.clone()),
                ))
            }
            Command::Serve {
                host,
                port,
                ocr_models,
                layout_model,
                unirec_models,
                vlm_url,
                vlm_model,
                vlm_api_key,
            } => {
                return server::serve(
                    host,
                    *port,
                    EnhanceState::new(
                        ocr_models.clone(),
                        layout_model.clone(),
                        unirec_models.clone(),
                        vlm_config(vlm_url.clone(), vlm_model.clone(), vlm_api_key.clone()),
                    ),
                )
            }
        }
    }
    if cli.inputs.is_empty() {
        anyhow::bail!("missing input file or folder (see --help)");
    }

    // Speed visualization (stderr-only, TTY-gated). The clock starts now so the
    // end-of-run summary (and --stats wall time) covers parse + every phase.
    let run_start = std::time::Instant::now();
    let reporter = progress::Reporter::new(cli.progress, cli.quiet);

    // Batch when given a folder, several inputs, or an explicit --out-dir;
    // otherwise the classic single-file path (result to stdout or -o).
    let single = cli.inputs.len() == 1 && cli.inputs[0].is_file() && cli.out_dir.is_none();
    if !single {
        batch::run(&cli, &reporter)?;
        if cli.stats {
            resources::report(&reporter, run_start.elapsed());
        }
        return Ok(());
    }

    let input = &cli.inputs[0];
    let input_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);

    let models = RunModels::from_cli(&cli);
    let doc = parse_and_enhance(input, &cli, &models, Some(&reporter))?;

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

    // End-of-run speed summary (pages · MB · wall · pages/s · MB/s). No-op when
    // progress is disabled; printed to stderr so stdout stays pure data.
    reporter.finish(
        &input.file_name().map_or_else(
            || input.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        doc.pages.len(),
        input_bytes,
    );

    // OKF is a directory bundle, not a stream — handle it before render_doc.
    if matches!(cli.format, Format::Okf) {
        if cli.okf_tar {
            // Deterministic tar to stdout (for `| tar x` / upload).
            use std::io::Write;
            let bundle = docparse_core::okf::build(&doc, &okf_options(&cli, input));
            std::io::stdout().write_all(&bundle.to_tar())?;
        } else {
            let dir = cli.out.clone().unwrap_or_else(|| derived_okf_dir(input));
            let explicit = cli.out.is_some();
            write_okf_bundle(&doc, &cli, input, &dir, explicit)?;
        }
    } else {
        let rendered = render_doc(&doc, &cli)?;
        match &cli.out {
            Some(path) => std::fs::write(path, rendered)?,
            None => println!("{rendered}"),
        }
    }
    if cli.stats {
        resources::report(&reporter, run_start.elapsed());
    }
    Ok(())
}

/// Parse one input and apply every enabled enhancement phase, returning the
/// finished document (empty-table placeholders dropped). Shared by the
/// single-file path and the batch runner.
///
/// `reporter`: `Some` shows a per-phase spinner / OCR page bar and emits the
/// per-phase JSON count lines on stderr (single-file behavior); `None` runs
/// quiet — batch mode's file bar + aggregate report stand in. A phase that
/// doesn't apply to the input's format is skipped; failures propagate so the
/// caller can record them.
fn parse_and_enhance(
    input: &std::path::Path,
    cli: &Cli,
    models: &RunModels,
    reporter: Option<&progress::Reporter>,
) -> anyhow::Result<docparse_core::ir::Document> {
    let is_pdf = input
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false);
    let log = reporter.is_some();

    let mut doc = {
        let _g = reporter.map(|r| r.spinner("parse"));
        parse_path_with(input, cli.image_dir.is_some() || cli.image_embed)?
    };

    if let Some(dir) = &cli.image_dir {
        let n = export_images(&mut doc, dir)?;
        if log {
            eprintln!("{{\"images_exported\": {n}}}");
        }
    }
    if cli.image_embed {
        let n = embed_images(&mut doc);
        if log {
            eprintln!("{{\"images_embedded\": {n}}}");
        }
    }

    if cli.ocr {
        // Load models only when some page actually needs enhancement — a
        // fully digital document with --ocr must stay zero-cost (and must not
        // fail on a missing model dir it would never use).
        let needs = docparse_core::quality::assess_pages(&doc)
            .iter()
            .any(|a| a.needs_enhancement);
        if needs {
            // Loaded once per run and cached (lazy): a batch of scans reads the
            // model on the first scanned page, not once per file.
            let ocr = models
                .ocr
                .get()
                .map_err(|e| anyhow::anyhow!("ocr models unavailable: {e}"))?;
            let ocr: &dyn docparse_core::enhance::Enhancer = ocr;
            let (enhanced, report) = match reporter {
                Some(r) => {
                    let (bar, _g) = r.page_bar("ocr", doc.pages.len() as u64);
                    match &bar {
                        Some(b) => {
                            let b = b.clone();
                            let on_page = move || b.inc(1);
                            docparse_core::enhance::apply_with(&doc, &[ocr], Some(&on_page))
                        }
                        None => docparse_core::enhance::apply(&doc, &[ocr]),
                    }
                }
                None => docparse_core::enhance::apply(&doc, &[ocr]),
            };
            doc = enhanced;
            if log {
                eprintln!("{}", docparse_core::enhance::report_json(&report));
            }
        } else if log {
            eprintln!("[]");
        }
    }

    if cli.layout {
        if is_pdf {
            let pdf_bytes = std::fs::read(input)?;
            let layout = models.layout.get(&cli.layout_model)?;
            let n = {
                let _g = reporter.map(|r| r.spinner("layout"));
                docparse_ocr::layout::enhance_document(&mut doc, pdf_bytes, layout, 2.0)?
            };
            if log {
                eprintln!("{{\"layout_enhanced_pages\": {n}}}");
            }
        } else if log {
            eprintln!("--layout currently supports PDF inputs only; skipped");
        }
    }

    if let Some(dir) = &cli.table_model {
        if !is_pdf {
            if log {
                eprintln!("--table-model currently supports PDF inputs only; skipped");
            }
        } else {
            let model = models.table.get(dir)?;
            let n = {
                let _g = reporter.map(|r| r.spinner("table"));
                docparse_ocr::table_model::refine_tables(&mut doc, std::fs::read(input)?, model)?
            };
            if log {
                eprintln!("{{\"table_model_refined\": {n}}}");
            }
        }
    }

    if let Some(dir) = &cli.formula_model {
        if !is_pdf {
            if log {
                eprintln!("--formula-model currently supports PDF inputs only; skipped");
            }
        } else {
            let layout = models.layout.get(&cli.layout_model)?;
            let model = models.formula.get(dir)?;
            let n = {
                let _g = reporter.map(|r| r.spinner("formula"));
                docparse_ocr::formula::enhance_formulas(
                    &mut doc,
                    std::fs::read(input)?,
                    layout,
                    model,
                )?
            };
            if log {
                eprintln!("{{\"formula_model_replaced\": {n}}}");
            }
        }
    }

    if let Some(dir) = &cli.transcribe_model {
        if !is_pdf {
            if log {
                eprintln!("--transcribe-model currently supports PDF inputs only; skipped");
            }
        } else {
            let layout = models.layout.get(&cli.layout_model)?;
            let model = models.transcribe.get(dir)?;
            let n = {
                let _g = reporter.map(|r| r.spinner("transcribe"));
                docparse_ocr::transcribe::transcribe_pages(
                    &mut doc,
                    std::fs::read(input)?,
                    layout,
                    model,
                )?
            };
            if log {
                eprintln!("{{\"transcribed_pages\": {n}}}");
            }
        }
    }

    if cli.vlm_describe || cli.vlm_tables {
        if !is_pdf {
            if log {
                eprintln!("--vlm-describe/--vlm-tables currently support PDF inputs only; skipped");
            }
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
                let n = {
                    let _g = reporter.map(|r| r.spinner("vlm-describe"));
                    docparse_vlm::annotate_pictures(&mut doc, std::fs::read(input)?, &client)?
                };
                if log {
                    eprintln!("{{\"vlm_described_figures\": {n}}}");
                }
            }
            if cli.vlm_tables {
                let n = {
                    let _g = reporter.map(|r| r.spinner("vlm-tables"));
                    docparse_vlm::refine_tables(&mut doc, std::fs::read(input)?, &client)?
                };
                if log {
                    eprintln!("{{\"vlm_refined_tables\": {n}}}");
                }
            }
        }
    }

    // After all enhancers: drop empty-row table placeholders before any output
    // or quality/profile pass sees them.
    drop_empty_table_placeholders(&mut doc);
    Ok(doc)
}

/// Render a finished document into the requested output format. Shared by the
/// single-file path and the batch runner.
fn render_doc(doc: &docparse_core::ir::Document, cli: &Cli) -> anyhow::Result<String> {
    Ok(match cli.format {
        Format::Json => output::to_json(doc)?,
        Format::Markdown => output::to_markdown(doc),
        Format::Text => output::to_text(doc),
        Format::Chunks => {
            let opts = docparse_core::chunk::ChunkOptions {
                table_markdown: matches!(cli.table_format, TableFormat::Markdown),
                ..Default::default()
            };
            docparse_core::chunk::to_json(&docparse_core::chunk::chunk_document_with(doc, opts))
        }
        Format::Outline => docparse_core::outline::to_json(&docparse_core::outline::build(doc)),
        // OKF writes a directory bundle, never a string — handled out-of-band.
        Format::Okf => unreachable!("okf is written via write_okf_bundle, not render_doc"),
    })
}

/// Auto-derived OKF bundle directory for `input`: `<stem>-okf/` in the cwd.
fn derived_okf_dir(input: &std::path::Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".into());
    PathBuf::from(format!("{stem}-okf"))
}

/// Build the OKF options for `input` (basename + mtime + resource base) and
/// write the bundle under `dir`. An auto-derived (`!explicit`) non-empty dir is
/// refused unless `--force`; an explicit `-o` dir is trusted.
fn write_okf_bundle(
    doc: &docparse_core::ir::Document,
    cli: &Cli,
    input: &std::path::Path,
    dir: &std::path::Path,
    explicit: bool,
) -> anyhow::Result<()> {
    if !explicit && !cli.force && dir_nonempty(dir) {
        anyhow::bail!(
            "{} exists and is not empty; use -o to target it or --force to overwrite",
            dir.display()
        );
    }
    let opts = okf_options(cli, input);
    let bundle = docparse_core::okf::build(doc, &opts);
    let concepts = bundle
        .files
        .iter()
        .filter(|(p, _)| p.file_name().and_then(|n| n.to_str()) != Some("index.md"))
        .count();
    bundle.write_to(dir)?;
    eprintln!(
        "wrote OKF bundle to {}/ ({concepts} concept(s))",
        dir.display()
    );
    Ok(())
}

/// Assemble [`docparse_core::okf::OkfOptions`] from the CLI + source file: the
/// basename for `resource` URIs and the file's mtime as a deterministic
/// ISO 8601 timestamp (never the wall clock).
pub(crate) fn okf_options(cli: &Cli, input: &std::path::Path) -> docparse_core::okf::OkfOptions {
    okf_options_for(
        input,
        cli.okf_resource_base.clone().unwrap_or_default(),
        matches!(cli.table_format, TableFormat::Markdown),
    )
}

/// `OkfOptions` from a source path alone (basename + mtime timestamp) — shared
/// by the CLI and the MCP/REST `okf` surfaces, which have no `Cli`.
pub(crate) fn okf_options_for(
    input: &std::path::Path,
    resource_base: String,
    table_markdown: bool,
) -> docparse_core::okf::OkfOptions {
    let source_name = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let timestamp = std::fs::metadata(input)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| iso8601_utc(d.as_secs()));
    docparse_core::okf::OkfOptions {
        resource_base,
        source_name,
        timestamp,
        table_markdown,
    }
}

/// True if `dir` exists and contains at least one entry.
fn dir_nonempty(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC), dependency-free via the
/// days-from-civil algorithm — deterministic, so bundles stay byte-identical.
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days (epoch 1970-01-01).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
