//! CLI progress & speed visualization — what makes the project's "fast" claim
//! *visible* on the command line.
//!
//! Why a dedicated module: the CLI one-shot path is a sequence of discrete
//! phases (parse → ocr → layout → table → …). Each phase is timed here and the
//! run ends with a speed summary (pages, MB, wall time, pages/s, MB/s). The
//! slow OCR phase additionally gets a determinate page bar fed from the
//! page-parallel `enhance::apply_with` callback.
//!
//! Channel discipline (CLAUDE.md invariant): all of this goes to **stderr** —
//! stdout is the data output (`-f json|markdown|…`). Gated on
//! `stderr().is_terminal()` in `auto` mode so pipes / redirects / CI / the
//! MCP & REST server paths stay byte-clean. `indicatif` draws to stderr and its
//! `ProgressBar` is internally `Arc`-shared, so the rayon OCR workers can
//! `inc(1)` it concurrently.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

/// When to show progress. `Auto` = only when stderr is a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ProgressMode {
    /// Show only when stderr is an interactive terminal (default).
    Auto,
    /// Always show, even when stderr is redirected.
    Always,
    /// Never show.
    Never,
    /// Emit machine-readable JSON-lines events to stderr instead of the human
    /// spinner/bar/table — for CI and wrapper tooling. No ANSI, no progress bar.
    Json,
}

/// Owns the run clock, the per-phase timing log, and the output decision. Human
/// progress (`human`) and JSON events (`json`) are mutually exclusive: `--progress
/// json` turns the human UI off and the event stream on.
pub struct Reporter {
    human: bool,
    json: bool,
    started: Instant,
    phases: RefCell<Vec<(&'static str, Duration)>>,
}

impl Reporter {
    /// Resolve `auto` against the real terminal and build the reporter. The run
    /// clock starts here, so construct it right after arg parsing.
    pub fn new(mode: ProgressMode, quiet: bool) -> Self {
        let human = !quiet
            && match mode {
                ProgressMode::Always => true,
                ProgressMode::Never | ProgressMode::Json => false,
                ProgressMode::Auto => std::io::stderr().is_terminal(),
            };
        let json = !quiet && mode == ProgressMode::Json;
        Reporter {
            human,
            json,
            started: Instant::now(),
            phases: RefCell::new(Vec::new()),
        }
    }

    /// Whether the human UI (spinner/bar/table) is on. Batch mode uses this to
    /// decide whether to print the human-readable report table to stderr.
    pub fn enabled(&self) -> bool {
        self.human
    }

    /// Whether machine-readable JSON-lines events are on (`--progress json`).
    pub fn json(&self) -> bool {
        self.json
    }

    /// Emit one JSON-lines event to stderr — only in `--progress json` mode.
    /// `serde_json::Value`'s `Display` is compact single-line JSON.
    pub fn emit(&self, event: &serde_json::Value) {
        if self.json {
            eprintln!("{event}");
        }
    }

    /// Determinate bar over a batch of files (top-level progress for folder /
    /// multi-input runs). Like [`page_bar`](Self::page_bar) but counts files.
    pub fn files_bar(&self, total: u64) -> Option<ProgressBar> {
        self.human.then(|| {
            let pb = ProgressBar::new(total);
            pb.set_style(
                ProgressStyle::with_template(
                    "{msg} [{bar:30.cyan/blue}] {pos}/{len} files · {per_sec} · ETA {eta}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
            );
            pb.set_message("batch");
            pb
        })
    }

    /// Spinner for a phase whose length we can't know up front (e.g. base parse
    /// — `DocumentParser::parse` is one-shot and reveals the page count only on
    /// return). The returned guard records the phase's elapsed time and clears
    /// the spinner on drop, so scope it tightly around the heavy call and emit
    /// any post-phase stderr line *after* the guard drops.
    pub fn spinner(&self, name: &'static str) -> PhaseGuard<'_> {
        let bar = self.human.then(|| {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg} {elapsed}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            pb.set_message(name);
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        });
        PhaseGuard {
            reporter: self,
            name,
            bar,
            start: Instant::now(),
        }
    }

    /// Determinate page bar (total pages known) for the slow OCR phase. Returns
    /// the bar to feed to the parallel loop's per-page callback plus a guard
    /// that records elapsed time and clears the bar on drop. Both share the same
    /// underlying `Arc`, so the workers' `inc(1)` and the guard's
    /// `finish_and_clear()` act on one bar.
    pub fn page_bar(
        &self,
        name: &'static str,
        total: u64,
    ) -> (Option<ProgressBar>, PhaseGuard<'_>) {
        let bar = self.human.then(|| {
            let pb = ProgressBar::new(total);
            pb.set_style(
                ProgressStyle::with_template(
                    "{msg} [{bar:30.cyan/blue}] {pos}/{len} pages · {per_sec} · ETA {eta}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
            );
            pb.set_message(name);
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        });
        let guard = PhaseGuard {
            reporter: self,
            name,
            bar: bar.clone(),
            start: Instant::now(),
        };
        (bar, guard)
    }

    /// End-of-run speed summary to stderr (single-file path). Human mode prints
    /// the `✓ …` line (+ phase breakdown); JSON mode emits one `summary` event.
    /// No-op when both are off. Call after every phase guard has dropped.
    pub fn finish(&self, label: &str, pages: usize, bytes: u64) {
        if !self.human && !self.json {
            return;
        }
        let secs = self.started.elapsed().as_secs_f64().max(1e-6);
        let mb = bytes as f64 / 1_048_576.0;
        if self.human {
            eprintln!(
                "✓ {label} · {pages} pages · {mb:.2} MB · {secs:.2}s · {:.1} pages/s · {:.1} MB/s",
                pages as f64 / secs,
                mb / secs,
            );
            let phases = self.phases.borrow();
            if phases.len() > 1 {
                let parts: Vec<String> = phases
                    .iter()
                    .map(|(n, d)| format!("{n} {:.2}s", d.as_secs_f64()))
                    .collect();
                eprintln!("  {}", parts.join(" · "));
            }
        }
        if self.json {
            self.emit(&serde_json::json!({
                "event": "summary",
                "scope": "file",
                "file": label,
                "pages": pages,
                "bytes": bytes,
                "seconds": (secs * 1000.0).round() / 1000.0,
                "pages_per_sec": (pages as f64 / secs * 10.0).round() / 10.0,
                "mb_per_sec": (mb / secs * 10.0).round() / 10.0,
            }));
        }
    }
}

/// Records a phase's elapsed time on drop and clears its bar/spinner. Hold it in
/// a tight scope around the phase's heavy work.
pub struct PhaseGuard<'a> {
    reporter: &'a Reporter,
    name: &'static str,
    bar: Option<ProgressBar>,
    start: Instant,
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
        self.reporter
            .phases
            .borrow_mut()
            .push((self.name, self.start.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_decides_enabled_deterministically() {
        // `always` forces on, `never` forces off — independent of the terminal.
        assert!(Reporter::new(ProgressMode::Always, false).enabled());
        assert!(!Reporter::new(ProgressMode::Never, false).enabled());
        // `--quiet` wins over any mode.
        assert!(!Reporter::new(ProgressMode::Always, true).enabled());
        assert!(!Reporter::new(ProgressMode::Never, true).enabled());
        assert!(!Reporter::new(ProgressMode::Auto, true).enabled());
    }

    #[test]
    fn disabled_reporter_makes_no_bars() {
        // When off, spinner/page_bar/files_bar produce no drawing handle, so the
        // pipeline runs with zero progress side effects.
        let r = Reporter::new(ProgressMode::Never, false);
        assert!(r.files_bar(10).is_none());
        let (bar, _g) = r.page_bar("ocr", 5);
        assert!(bar.is_none());
    }

    #[test]
    fn json_mode_is_machine_not_human() {
        // `--progress json`: events on, human UI off (no bars/ANSI).
        let r = Reporter::new(ProgressMode::Json, false);
        assert!(r.json());
        assert!(!r.enabled(), "json mode turns the human UI off");
        assert!(r.files_bar(3).is_none(), "no bar in json mode");
        // --quiet silences even json.
        let q = Reporter::new(ProgressMode::Json, true);
        assert!(!q.json());
        assert!(!q.enabled());
    }
}
