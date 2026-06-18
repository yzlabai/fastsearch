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
}

/// Owns the run clock, the per-phase timing log, and the on/off decision.
pub struct Reporter {
    enabled: bool,
    started: Instant,
    phases: RefCell<Vec<(&'static str, Duration)>>,
}

impl Reporter {
    /// Resolve `auto` against the real terminal and build the reporter. The run
    /// clock starts here, so construct it right after arg parsing.
    pub fn new(mode: ProgressMode, quiet: bool) -> Self {
        let enabled = !quiet
            && match mode {
                ProgressMode::Always => true,
                ProgressMode::Never => false,
                ProgressMode::Auto => std::io::stderr().is_terminal(),
            };
        Reporter {
            enabled,
            started: Instant::now(),
            phases: RefCell::new(Vec::new()),
        }
    }

    /// Whether progress is on (TTY-resolved). Batch mode uses this to decide
    /// whether to print the human-readable report table to stderr.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Determinate bar over a batch of files (top-level progress for folder /
    /// multi-input runs). Like [`page_bar`](Self::page_bar) but counts files.
    pub fn files_bar(&self, total: u64) -> Option<ProgressBar> {
        self.enabled.then(|| {
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
        let bar = self.enabled.then(|| {
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
        let bar = self.enabled.then(|| {
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

    /// Print the end-of-run speed summary to stderr. No-op when disabled. Call
    /// after every phase guard has dropped (no bar is drawing).
    pub fn finish(&self, label: &str, pages: usize, bytes: u64) {
        if !self.enabled {
            return;
        }
        let secs = self.started.elapsed().as_secs_f64().max(1e-6);
        let mb = bytes as f64 / 1_048_576.0;
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
}
