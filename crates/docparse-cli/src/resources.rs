//! `--stats`: CPU & peak-memory usage for a parse run.
//!
//! Read once at the end via `getrusage(RUSAGE_SELF)` (libc — already in the tree
//! transitively, so zero new supply-chain surface): peak resident set size +
//! cumulative CPU time (user + sys) across all threads. Average CPU utilization
//! = CPU time / wall time; **>100% is expected and good** — it means the
//! page-parallel parse / OCR actually used multiple cores. Printed to stderr in
//! human form, or emitted as a JSON `resources` event under `--progress json`.
//!
//! `--stats` is an explicit opt-in, so the human line prints regardless of the
//! `--progress` setting (like `--quality`/`--profile`); only `--progress json`
//! redirects it to an event so the JSON stream stays pure.

use std::time::Duration;

use crate::progress::Reporter;

struct Sample {
    available: bool,
    peak_rss_bytes: u64,
    cpu_user: f64,
    cpu_sys: f64,
}

#[cfg(unix)]
fn sample() -> Sample {
    // SAFETY: getrusage fills a plain C struct we zero-initialize; no aliasing.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    if rc != 0 {
        return Sample {
            available: false,
            peak_rss_bytes: 0,
            cpu_user: 0.0,
            cpu_sys: 0.0,
        };
    }
    // ru_maxrss units differ by OS: bytes on macOS, kilobytes on Linux.
    let maxrss = u.ru_maxrss.max(0) as u64;
    let peak_rss_bytes = if cfg!(target_os = "macos") {
        maxrss
    } else {
        maxrss * 1024
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    Sample {
        available: true,
        peak_rss_bytes,
        cpu_user: secs(u.ru_utime),
        cpu_sys: secs(u.ru_stime),
    }
}

#[cfg(not(unix))]
fn sample() -> Sample {
    Sample {
        available: false,
        peak_rss_bytes: 0,
        cpu_user: 0.0,
        cpu_sys: 0.0,
    }
}

/// Report CPU & peak-memory usage. `wall` is the whole-run elapsed time.
pub fn report(reporter: &Reporter, wall: Duration) {
    let s = sample();
    let wall_s = wall.as_secs_f64().max(1e-6);
    if reporter.json() {
        reporter.emit(&json_value(&s, wall_s));
    } else {
        eprintln!("{}", human_line(&s, wall_s));
    }
}

/// The human-readable stderr line (or an `unavailable` notice). Pure, so it's
/// unit-testable without capturing stderr.
fn human_line(s: &Sample, wall_s: f64) -> String {
    if !s.available {
        return "resources: CPU/memory stats unavailable on this platform".to_string();
    }
    let cpu = s.cpu_user + s.cpu_sys;
    let util = cpu / wall_s * 100.0;
    let mb = s.peak_rss_bytes as f64 / 1_048_576.0;
    format!(
        "resources: peak RSS {mb:.1} MB · CPU {cpu:.2}s (user {:.2} + sys {:.2}) · {util:.0}% util · wall {wall_s:.2}s",
        s.cpu_user, s.cpu_sys,
    )
}

/// The `resources` JSON event. Pure, mirrors [`human_line`]'s numbers.
fn json_value(s: &Sample, wall_s: f64) -> serde_json::Value {
    let cpu = s.cpu_user + s.cpu_sys;
    serde_json::json!({
        "event": "resources",
        "available": s.available,
        "peak_rss_bytes": s.peak_rss_bytes,
        "cpu_user_seconds": round3(s.cpu_user),
        "cpu_sys_seconds": round3(s.cpu_sys),
        "cpu_seconds": round3(cpu),
        "cpu_util_percent": (cpu / wall_s * 100.0 * 10.0).round() / 10.0,
        "wall_seconds": round3(wall_s),
    })
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(rss: u64, user: f64, sys: f64) -> Sample {
        Sample {
            available: true,
            peak_rss_bytes: rss,
            cpu_user: user,
            cpu_sys: sys,
        }
    }

    #[test]
    fn human_line_shows_rss_cpu_util() {
        // cpu = 2.00s over wall 1.0s → 200% util; 1 MiB RSS.
        let line = human_line(&ok(1_048_576, 1.5, 0.5), 1.0);
        assert!(line.contains("peak RSS 1.0 MB"), "{line}");
        assert!(line.contains("CPU 2.00s"), "{line}");
        assert!(line.contains("200% util"), "{line}");
    }

    #[test]
    fn unavailable_is_reported_both_ways() {
        let un = Sample {
            available: false,
            peak_rss_bytes: 0,
            cpu_user: 0.0,
            cpu_sys: 0.0,
        };
        assert!(human_line(&un, 1.0).contains("unavailable"));
        assert_eq!(json_value(&un, 1.0)["available"], false);
    }

    #[test]
    fn json_value_has_fields_and_util() {
        let v = json_value(&ok(2_097_152, 1.0, 1.0), 1.0);
        assert_eq!(v["event"], "resources");
        assert_eq!(v["available"], true);
        assert_eq!(v["peak_rss_bytes"], 2_097_152);
        assert_eq!(v["cpu_seconds"], 2.0);
        assert_eq!(v["cpu_util_percent"], 200.0);
    }

    /// Reading our own usage really works on this OS (peak RSS is nonzero).
    #[cfg(unix)]
    #[test]
    fn sample_reads_real_usage() {
        let s = sample();
        assert!(s.available);
        assert!(s.peak_rss_bytes > 0, "process should have nonzero RSS");
        assert!(s.cpu_user + s.cpu_sys >= 0.0);
    }
}
