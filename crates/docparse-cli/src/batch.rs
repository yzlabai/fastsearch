//! Batch mode — parse many files / a folder in one run and print an aggregate
//! report.
//!
//! Triggered (from `main`) by a directory input, more than one input, or an
//! explicit `--out-dir`. Each input goes through the same pipeline as the
//! single-file path ([`crate::parse_and_enhance`] + [`crate::render_doc`]);
//! results are written one-per-input under `--out-dir`, and an aggregate
//! report (human table → stderr, optional JSON / CSV → file) summarizes pages,
//! size, time, and per-file status.
//!
//! **Serial by default, opt-in file-level parallelism (`--jobs N`).** Each file
//! already parses page-parallel (and OCR runs on a memory-bounded pool), so the
//! default is one file at a time with every core already busy within a file.
//! `--jobs N` adds file-level parallelism for *deterministic* batches (lots of
//! small digital PDFs, where per-file page parallelism can't saturate cores);
//! it is force-disabled whenever a model flag is set, because per-page scan
//! buffers + ~700MB models would multiply across files and blow the memory
//! bound (see [`effective_jobs`]). A bad file is recorded as an error row and
//! never aborts the batch — the file-level analogue of the "bad page → empty
//! Page, never panic" invariant.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use crate::{
    parse_and_enhance, parsers_with, progress::Reporter, render_doc, Cli, Format, RunModels,
};

/// One discovered input: the file to parse plus the path to mirror under
/// `--out-dir`. `rel` is the file's path relative to the folder it was found in
/// (just the filename for top-level / explicitly-named files), so a recursive
/// run writes `out/sub/x.pdf.json` instead of flattening every `x.pdf` onto one
/// name.
struct BatchInput {
    path: PathBuf,
    rel: PathBuf,
}

/// One file's outcome in the report.
struct FileStat {
    /// Full source path (report `path` field).
    path: PathBuf,
    /// Path relative to the input folder — the report/table label and what was
    /// written under `--out-dir`. Disambiguates same-named files across sub-dirs.
    rel: PathBuf,
    bytes: u64,
    pages: usize,
    secs: f64,
    error: Option<String>,
}

impl FileStat {
    fn label(&self) -> String {
        self.rel.to_string_lossy().into_owned()
    }
}

/// Run the batch: collect files, process each, emit the report(s).
pub fn run(cli: &Cli, reporter: &Reporter) -> anyhow::Result<()> {
    let files = collect_files(&cli.inputs, cli.recursive)?;
    if files.is_empty() {
        anyhow::bail!("no supported files found in the given input(s)");
    }
    if let Some(dir) = &cli.out_dir {
        std::fs::create_dir_all(dir)?;
    } else {
        // No --out-dir: we can't emit N results to stdout sensibly, so this is a
        // parse-and-report run. Say so rather than silently discarding content.
        eprintln!(
            "note: no --out-dir — parsing {} file(s) for the report only; parsed content is not written",
            files.len()
        );
    }

    // Load OCR / UniRec models once for the whole batch (lazy: a digital-only or
    // no-model run never touches them), not once per file.
    let models = RunModels::from_cli(cli);

    // File-level parallelism (--jobs), force-disabled for model batches.
    let cores = std::thread::available_parallelism()
        .map(|c| c.get())
        .unwrap_or(1);
    let jobs = effective_jobs(cli.jobs, cores, files.len(), any_model_flag(cli));
    if cli.jobs > 1 && any_model_flag(cli) {
        eprintln!(
            "note: --jobs {} ignored — a model flag is set; files run serially to bound memory",
            cli.jobs
        );
    }

    let bar = reporter.files_bar(files.len() as u64);
    let started = Instant::now();

    // Parse one input into a FileStat (parse + write are independent per file,
    // so this is safe to run concurrently). Quiet pipeline (reporter = None):
    // the file bar + report are the UI. A parse failure becomes an error row;
    // the batch never aborts. The bar (thread-safe) ticks as each file lands.
    let process = |inp: &BatchInput| -> FileStat {
        let bytes = std::fs::metadata(&inp.path).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        let stat = match parse_and_enhance(&inp.path, cli, &models, None) {
            Ok(doc) => FileStat {
                path: inp.path.clone(),
                rel: inp.rel.clone(),
                bytes,
                pages: doc.pages.len(),
                secs: t.elapsed().as_secs_f64(),
                error: write_output(cli, &inp.rel, &doc)
                    .err()
                    .map(|e| format!("write: {e}")),
            },
            Err(e) => FileStat {
                path: inp.path.clone(),
                rel: inp.rel.clone(),
                bytes,
                pages: 0,
                secs: t.elapsed().as_secs_f64(),
                error: Some(short_err(&e)),
            },
        };
        if let Some(b) = &bar {
            b.inc(1);
        }
        stat
    };

    // Indexed collection preserves input order regardless of completion order,
    // so the report / JSON events are deterministic. jobs==1 keeps the exact
    // serial path (output byte-identical to before --jobs existed).
    let stats: Vec<FileStat> = if jobs > 1 {
        match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
            Ok(pool) => pool.install(|| files.par_iter().map(&process).collect()),
            Err(_) => files.iter().map(&process).collect(), // OS refused threads
        }
    } else {
        files.iter().map(&process).collect()
    };

    // Per-file events stream after collection (order-stable, no interleaving
    // across threads on stderr) — schema matches --report-json's file objects.
    if reporter.json() {
        for stat in &stats {
            let mut ev = file_value(stat);
            ev["event"] = serde_json::Value::String("file".into());
            reporter.emit(&ev);
        }
    }
    if let Some(b) = &bar {
        b.finish_and_clear();
    }

    let total_secs = started.elapsed().as_secs_f64();

    // Human table → stderr (gated on progress on/off so --quiet stays quiet).
    if reporter.enabled() {
        eprint!("{}", render_table(&stats, total_secs));
    }
    // Machine-readable batch summary event (--progress json).
    if reporter.json() {
        let mut ev = totals_value(&totals(&stats), total_secs);
        ev["event"] = serde_json::Value::String("summary".into());
        ev["scope"] = serde_json::Value::String("batch".into());
        reporter.emit(&ev);
    }
    if let Some(p) = &cli.report_json {
        std::fs::write(p, render_json(&stats, total_secs))?;
    }
    if let Some(p) = &cli.report_csv {
        std::fs::write(p, render_csv(&stats))?;
    }
    Ok(())
}

/// Write one input's rendered output under `--out-dir` at `<rel>.<format-ext>`
/// (e.g. `sub/report.pdf` → `out/sub/report.pdf.json`). Keeping the full original
/// name avoids `a.pdf`/`a.docx` colliding on `a.json`; mirroring `rel`'s sub-dirs
/// avoids same-named files in different folders colliding in a recursive run.
/// No-op when there's no `--out-dir` (report-only run).
fn write_output(cli: &Cli, rel: &Path, doc: &docparse_core::ir::Document) -> anyhow::Result<()> {
    let Some(dir) = &cli.out_dir else {
        return Ok(());
    };
    let rendered = render_doc(doc, cli)?;
    let rel = safe_rel(rel);
    let target = dir.join(format!("{}.{}", rel.display(), output_ext(cli.format)));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(target, rendered)?;
    Ok(())
}

/// Keep an output sub-path inside `--out-dir`. `rel` is built from a relative
/// `strip_prefix` or a bare file name, so this is belt-and-suspenders: reject
/// anything absolute or containing `..`, falling back to the bare file name (or
/// `out` if even that is absent). Prevents a pathological input from escaping
/// the output directory via `dir.join(rel)`.
fn safe_rel(rel: &Path) -> PathBuf {
    use std::path::Component;
    let safe = rel.is_relative()
        && rel.components().all(|c| {
            !matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if safe {
        rel.to_path_buf()
    } else {
        rel.file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("out"))
    }
}

/// File extension for a rendered output (chunks are JSON too).
fn output_ext(format: Format) -> &'static str {
    match format {
        Format::Json | Format::Chunks => "json",
        Format::Markdown => "md",
        Format::Text => "txt",
    }
}

/// Expand inputs into a sorted, de-duplicated list. Explicit file inputs are
/// always included (the user named them) and write under their bare filename;
/// directory contents are filtered to supported extensions and carry their path
/// relative to the input folder so `--out-dir` can mirror sub-dirs. `recursive`
/// descends into sub-folders.
fn collect_files(inputs: &[PathBuf], recursive: bool) -> anyhow::Result<Vec<BatchInput>> {
    let probe = parsers_with(false);
    let supported = |p: &Path| probe.iter().any(|parser| parser.supports(p));
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            collect_dir(input, input, recursive, &supported, &mut out)?;
        } else if input.is_file() {
            out.push(BatchInput {
                rel: PathBuf::from(input.file_name().unwrap_or(input.as_os_str())),
                path: input.clone(),
            });
        } else {
            anyhow::bail!("input not found: {}", input.display());
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// `base` is the directory the user named — `rel` is each file's path relative
/// to it, so nested files keep their sub-path under `--out-dir`.
fn collect_dir(
    dir: &Path,
    base: &Path,
    recursive: bool,
    supported: &dyn Fn(&Path) -> bool,
    out: &mut Vec<BatchInput>,
) -> anyhow::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        // `is_dir()` follows symlinks, so a symlinked directory (or a cycle like
        // `a/link -> a`) would recurse forever and blow the stack. Don't follow
        // symlinked dirs — the safe default; a real dir tree still walks fully.
        if path.is_dir() && !path.is_symlink() {
            if recursive {
                collect_dir(&path, base, recursive, supported, out)?;
            }
        } else if path.is_file() && supported(&path) {
            let rel = path
                .strip_prefix(base)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| PathBuf::from(path.file_name().unwrap_or(path.as_os_str())));
            out.push(BatchInput {
                path: path.clone(),
                rel,
            });
        }
    }
    Ok(())
}

/// First line of an error chain — keeps the report row to one line.
fn short_err(e: &anyhow::Error) -> String {
    e.to_string().lines().next().unwrap_or("error").to_string()
}

/// Whether any enhancement model is enabled — file-level parallelism must stay
/// off in that case (per-page scan buffers + ~700MB models multiply per file).
fn any_model_flag(cli: &Cli) -> bool {
    cli.ocr
        || cli.layout
        || cli.table_model.is_some()
        || cli.formula_model.is_some()
        || cli.transcribe_model.is_some()
        || cli.vlm_describe
        || cli.vlm_tables
}

/// Resolve `--jobs` to an actual worker count: forced to 1 for model batches
/// (memory), otherwise the request clamped to both the core count and the file
/// count (no point spawning more workers than files). Pure for testability.
fn effective_jobs(requested: usize, cores: usize, files: usize, has_model: bool) -> usize {
    if has_model {
        return 1;
    }
    requested.max(1).min(cores.max(1)).min(files.max(1))
}

// --- report rendering -------------------------------------------------------

struct Totals {
    files: usize,
    ok: usize,
    failed: usize,
    pages: usize,
    bytes: u64,
}

fn totals(stats: &[FileStat]) -> Totals {
    let ok = stats.iter().filter(|f| f.error.is_none()).count();
    Totals {
        files: stats.len(),
        ok,
        failed: stats.len() - ok,
        pages: stats.iter().map(|f| f.pages).sum(),
        bytes: stats.iter().map(|f| f.bytes).sum(),
    }
}

const MB: f64 = 1_048_576.0;

fn render_table(stats: &[FileStat], total_secs: f64) -> String {
    // Width on char count (good enough; CJK double-width names may still drift).
    let name_w = stats
        .iter()
        .map(|f| f.label().chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 50);
    let mut s = String::new();
    s.push_str(&format!(
        "{:<name_w$}  {:>5}  {:>7}  {:>7}  {:>9}  {}\n",
        "file", "pages", "MB", "time", "pages/s", "status"
    ));
    for f in stats {
        let (pages, pps) = if f.error.is_some() {
            ("—".to_string(), "—".to_string())
        } else {
            (
                f.pages.to_string(),
                format!("{:.1}", f.pages as f64 / f.secs.max(1e-6)),
            )
        };
        let status = match &f.error {
            None => "ok".to_string(),
            Some(e) => format!("ERROR: {e}"),
        };
        s.push_str(&format!(
            "{:<name_w$}  {:>5}  {:>7.2}  {:>6.2}s  {:>9}  {}\n",
            truncate(&f.label(), name_w),
            pages,
            f.bytes as f64 / MB,
            f.secs,
            pps,
            status,
        ));
    }
    let t = totals(stats);
    s.push_str(&format!("{}\n", "─".repeat(name_w + 40)));
    s.push_str(&format!(
        "{} files · {} ok · {} failed · {} pages · {:.2} MB · {:.2}s · {:.1} pages/s\n",
        t.files,
        t.ok,
        t.failed,
        t.pages,
        t.bytes as f64 / MB,
        total_secs,
        t.pages as f64 / total_secs.max(1e-6),
    ));
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// One file's JSON object — shared by the `--report-json` file and the
/// `--progress json` `file` event so both carry an identical schema.
fn file_value(f: &FileStat) -> serde_json::Value {
    let mut o = serde_json::json!({
        "file": f.label(),
        "path": f.path.display().to_string(),
        "bytes": f.bytes,
        "pages": f.pages,
        "seconds": round3(f.secs),
        "ok": f.error.is_none(),
    });
    if let Some(e) = &f.error {
        o["error"] = serde_json::Value::String(e.clone());
    }
    o
}

/// Batch totals as a JSON object — shared by the report and the `summary` event.
fn totals_value(t: &Totals, total_secs: f64) -> serde_json::Value {
    serde_json::json!({
        "files": t.files,
        "ok": t.ok,
        "failed": t.failed,
        "pages": t.pages,
        "bytes": t.bytes,
        "seconds": round3(total_secs),
        "pages_per_sec": round1(t.pages as f64 / total_secs.max(1e-6)),
    })
}

fn render_json(stats: &[FileStat], total_secs: f64) -> String {
    let files: Vec<serde_json::Value> = stats.iter().map(file_value).collect();
    let report = serde_json::json!({
        "files": files,
        "totals": totals_value(&totals(stats), total_secs),
    });
    let mut out = serde_json::to_string_pretty(&report).unwrap_or_default();
    out.push('\n');
    out
}

fn render_csv(stats: &[FileStat]) -> String {
    let mut s = String::from("file,path,bytes,pages,seconds,ok,error\n");
    for f in stats {
        s.push_str(&format!(
            "{},{},{},{},{:.3},{},{}\n",
            csv_field(&f.label()),
            csv_field(&f.path.display().to_string()),
            f.bytes,
            f.pages,
            f.secs,
            f.error.is_none(),
            csv_field(f.error.as_deref().unwrap_or("")),
        ));
    }
    s
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(name: &str, pages: usize, bytes: u64, error: Option<&str>) -> FileStat {
        FileStat {
            path: PathBuf::from(name),
            rel: PathBuf::from(name),
            bytes,
            pages,
            secs: 0.1,
            error: error.map(|e| e.to_string()),
        }
    }

    #[test]
    fn effective_jobs_forces_serial_for_model_batches() {
        // A model flag pins jobs to 1 no matter what was requested.
        assert_eq!(effective_jobs(8, 16, 100, true), 1);
        // Deterministic: request honored but clamped to cores and file count.
        assert_eq!(effective_jobs(8, 16, 100, false), 8);
        assert_eq!(effective_jobs(32, 4, 100, false), 4); // core-bound
        assert_eq!(effective_jobs(8, 16, 3, false), 3); // file-bound
                                                        // Default / degenerate requests yield at least one worker.
        assert_eq!(effective_jobs(1, 16, 100, false), 1);
        assert_eq!(effective_jobs(0, 16, 100, false), 1);
    }

    /// A unique scratch dir under the system temp, following the repo's
    /// pid-suffixed convention (no tempfile crate in the tree).
    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("docparse-batch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn safe_rel_blocks_escape() {
        // Normal relative paths pass through unchanged.
        assert_eq!(safe_rel(Path::new("a.pdf")), PathBuf::from("a.pdf"));
        assert_eq!(safe_rel(Path::new("sub/a.pdf")), PathBuf::from("sub/a.pdf"));
        // Absolute or `..`-bearing paths fall back to the bare file name.
        assert_eq!(safe_rel(Path::new("/etc/passwd")), PathBuf::from("passwd"));
        assert_eq!(safe_rel(Path::new("../../x.pdf")), PathBuf::from("x.pdf"));
        assert_eq!(safe_rel(Path::new("a/../../b.pdf")), PathBuf::from("b.pdf"));
    }

    #[test]
    fn label_uses_relative_path() {
        // The report label is the relative path (disambiguates recursive dups),
        // not the bare file name.
        let mut s = stat("paper.pdf", 1, 10, None);
        s.rel = PathBuf::from("alpha/paper.pdf");
        assert_eq!(s.label(), "alpha/paper.pdf");
    }

    #[test]
    fn output_ext_maps_each_format() {
        assert_eq!(output_ext(Format::Json), "json");
        assert_eq!(output_ext(Format::Chunks), "json");
        assert_eq!(output_ext(Format::Markdown), "md");
        assert_eq!(output_ext(Format::Text), "txt");
    }

    #[test]
    fn csv_field_quotes_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("she said \"hi\""), "\"she said \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn truncate_is_char_aware() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // Counts chars, not bytes — multibyte names don't over-truncate.
        assert_eq!(truncate("café", 4), "café");
    }

    #[test]
    fn totals_counts_ok_and_failed() {
        let stats = vec![
            stat("a.pdf", 10, 100, None),
            stat("b.pdf", 0, 200, Some("boom")),
            stat("c.pdf", 5, 300, None),
        ];
        let t = totals(&stats);
        assert_eq!((t.files, t.ok, t.failed), (3, 2, 1));
        assert_eq!(t.pages, 15);
        assert_eq!(t.bytes, 600);
    }

    #[test]
    fn table_shows_rows_totals_and_errors() {
        let stats = vec![
            stat("good.pdf", 12, 1_048_576, None),
            stat("bad.pdf", 0, 2_097_152, Some("invalid xref")),
        ];
        let table = render_table(&stats, 0.5);
        assert!(table.contains("good.pdf"));
        assert!(table.contains("ERROR: invalid xref"));
        assert!(table.contains("2 files · 1 ok · 1 failed"));
        assert!(table.contains("12 pages"));
    }

    #[test]
    fn json_report_has_per_file_and_totals() {
        let stats = vec![
            stat("a.pdf", 3, 10, None),
            stat("b.pdf", 0, 20, Some("nope")),
        ];
        let v: serde_json::Value = serde_json::from_str(&render_json(&stats, 1.0)).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 2);
        assert_eq!(v["files"][0]["ok"], true);
        assert_eq!(v["files"][1]["ok"], false);
        assert_eq!(v["files"][1]["error"], "nope");
        assert_eq!(v["totals"]["files"], 2);
        assert_eq!(v["totals"]["ok"], 1);
        assert_eq!(v["totals"]["pages"], 3);
    }

    #[test]
    fn collect_files_filters_recurses_and_includes_explicit() {
        let root = scratch("collect");
        let sub = root.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("a.pdf"), b"x").unwrap();
        std::fs::write(root.join("b.docx"), b"x").unwrap();
        std::fs::write(root.join("notes.xyz"), b"x").unwrap(); // unsupported ext
        std::fs::write(sub.join("c.pdf"), b"x").unwrap();

        // Top level only: supported files, unsupported dropped, sub-dir skipped.
        let top = collect_files(std::slice::from_ref(&root), false).unwrap();
        let names: Vec<String> = top
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a.pdf", "b.docx"],
            "filtered + sorted, no sub-dir"
        );
        // Top-level files mirror to their bare filename.
        assert_eq!(top[0].rel, PathBuf::from("a.pdf"));

        // Recursive: pulls in the nested pdf, and its rel keeps the sub-dir so
        // same-named files in different folders won't collide in --out-dir.
        let deep = collect_files(std::slice::from_ref(&root), true).unwrap();
        assert_eq!(deep.len(), 3);
        let nested = deep
            .iter()
            .find(|i| i.path.ends_with("nested/c.pdf"))
            .expect("nested pdf collected");
        assert_eq!(nested.rel, PathBuf::from("nested/c.pdf"));

        // Explicit file is always included, even with an unsupported extension.
        let explicit = collect_files(&[root.join("notes.xyz")], false).unwrap();
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].rel, PathBuf::from("notes.xyz"));

        // Missing input is an error, not a silent skip.
        assert!(collect_files(&[root.join("ghost.pdf")], false).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_does_not_follow_symlink_cycles() {
        let root = scratch("symlink");
        std::fs::write(root.join("real.pdf"), b"x").unwrap();
        // A cycle: root/loop -> root. Following it would recurse forever.
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();

        // Terminates (no stack overflow) and doesn't re-collect via the link.
        let files = collect_files(std::slice::from_ref(&root), true).unwrap();
        assert_eq!(files.len(), 1, "real file once, symlinked dir not followed");
        assert!(files[0].path.ends_with("real.pdf"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
