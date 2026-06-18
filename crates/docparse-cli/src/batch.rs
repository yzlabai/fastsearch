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
//! **Sequential by design.** Each file already parses page-parallel (and OCR
//! runs on a memory-bounded pool); stacking file-level parallelism on top would
//! blow that memory bound. So files run one at a time, every core already busy
//! within a file. A bad file is recorded as an error row and never aborts the
//! batch — the file-level analogue of the "bad page → empty Page, never panic"
//! invariant.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{parse_and_enhance, parsers_with, progress::Reporter, render_doc, Cli, Format};

/// One file's outcome in the report.
struct FileStat {
    path: PathBuf,
    bytes: u64,
    pages: usize,
    secs: f64,
    error: Option<String>,
}

impl FileStat {
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
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

    let bar = reporter.files_bar(files.len() as u64);
    let started = Instant::now();
    let mut stats = Vec::with_capacity(files.len());

    for path in &files {
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let t = Instant::now();
        // Quiet pipeline (reporter = None): the file bar + report are the UI.
        // A parse failure becomes an error row; the batch never aborts.
        let stat = match parse_and_enhance(path, cli, None) {
            Ok(doc) => FileStat {
                path: path.clone(),
                bytes,
                pages: doc.pages.len(),
                secs: t.elapsed().as_secs_f64(),
                error: write_output(cli, path, &doc)
                    .err()
                    .map(|e| format!("write: {e}")),
            },
            Err(e) => FileStat {
                path: path.clone(),
                bytes,
                pages: 0,
                secs: t.elapsed().as_secs_f64(),
                error: Some(short_err(&e)),
            },
        };
        stats.push(stat);
        if let Some(b) = &bar {
            b.inc(1);
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
    if let Some(p) = &cli.report_json {
        std::fs::write(p, render_json(&stats, total_secs))?;
    }
    if let Some(p) = &cli.report_csv {
        std::fs::write(p, render_csv(&stats))?;
    }
    Ok(())
}

/// Write one input's rendered output under `--out-dir` as
/// `<original-filename>.<format-ext>` (e.g. `report.pdf` → `report.pdf.json`).
/// Keeping the full original name avoids `a.pdf`/`a.docx` colliding on `a.json`.
/// No-op when there's no `--out-dir` (report-only run).
fn write_output(cli: &Cli, src: &Path, doc: &docparse_core::ir::Document) -> anyhow::Result<()> {
    let Some(dir) = &cli.out_dir else {
        return Ok(());
    };
    let rendered = render_doc(doc, cli)?;
    let stem = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    std::fs::write(
        dir.join(format!("{stem}.{}", output_ext(cli.format))),
        rendered,
    )?;
    Ok(())
}

/// File extension for a rendered output (chunks are JSON too).
fn output_ext(format: Format) -> &'static str {
    match format {
        Format::Json | Format::Chunks => "json",
        Format::Markdown => "md",
        Format::Text => "txt",
    }
}

/// Expand inputs into a sorted, de-duplicated file list. Explicit file inputs
/// are always included (the user named them); directory contents are filtered
/// to extensions a backend supports. `recursive` descends into sub-folders.
fn collect_files(inputs: &[PathBuf], recursive: bool) -> anyhow::Result<Vec<PathBuf>> {
    let probe = parsers_with(false);
    let supported = |p: &Path| probe.iter().any(|parser| parser.supports(p));
    let mut out = Vec::new();
    for input in inputs {
        if input.is_dir() {
            collect_dir(input, recursive, &supported, &mut out)?;
        } else if input.is_file() {
            out.push(input.clone());
        } else {
            anyhow::bail!("input not found: {}", input.display());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_dir(
    dir: &Path,
    recursive: bool,
    supported: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if recursive {
                collect_dir(&path, recursive, supported, out)?;
            }
        } else if path.is_file() && supported(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// First line of an error chain — keeps the report row to one line.
fn short_err(e: &anyhow::Error) -> String {
    e.to_string().lines().next().unwrap_or("error").to_string()
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
        .map(|f| f.name().chars().count())
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
            truncate(&f.name(), name_w),
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

fn render_json(stats: &[FileStat], total_secs: f64) -> String {
    let files: Vec<serde_json::Value> = stats
        .iter()
        .map(|f| {
            let mut o = serde_json::json!({
                "file": f.name(),
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
        })
        .collect();
    let t = totals(stats);
    let report = serde_json::json!({
        "files": files,
        "totals": {
            "files": t.files,
            "ok": t.ok,
            "failed": t.failed,
            "pages": t.pages,
            "bytes": t.bytes,
            "seconds": round3(total_secs),
            "pages_per_sec": round1(t.pages as f64 / total_secs.max(1e-6)),
        }
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
            csv_field(&f.name()),
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
            bytes,
            pages,
            secs: 0.1,
            error: error.map(|e| e.to_string()),
        }
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
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a.pdf", "b.docx"],
            "filtered + sorted, no sub-dir"
        );

        // Recursive: pulls in the nested pdf too.
        let deep = collect_files(std::slice::from_ref(&root), true).unwrap();
        assert!(deep.iter().any(|p| p.ends_with("nested/c.pdf")));
        assert_eq!(deep.len(), 3);

        // Explicit file is always included, even with an unsupported extension.
        let explicit = collect_files(&[root.join("notes.xyz")], false).unwrap();
        assert_eq!(explicit.len(), 1);

        // Missing input is an error, not a silent skip.
        assert!(collect_files(&[root.join("ghost.pdf")], false).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }
}
