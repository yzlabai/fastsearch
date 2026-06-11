#!/usr/bin/env python3
"""G7 corpus stress run: sweep every parseable file under the local reference
corpora through the release binary, then a deterministic mutation pass
(truncations + byte flips) over a diverse PDF subset.

Gate: ZERO panics/crashes (exit 101 / signals). Clean errors ("failed to
parse") are acceptable and counted separately — refusing malformed input
gracefully is correct behavior; dying is not.

Usage: python3 scripts/stress.py [--mutations N]   # writes a markdown report to stdout
"""
import argparse, hashlib, os, signal, subprocess, sys, tempfile, time
from pathlib import Path

BIN = Path("target/release/docparse")
ROOTS = [Path(".."), Path("tmp")]
EXTS = ["pdf", "docx", "xlsx", "pptx", "html", "md", "csv", "srt", "vtt", "tex", "eml", "png", "jpg", "jpeg"]
SKIP_DIRS = {"target", "node_modules", ".git"}
TIMEOUT = 120  # generous: stress is about crashes, not speed gates


def find_corpus():
    seen = {}
    for root in ROOTS:
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
            for f in filenames:
                ext = f.rsplit(".", 1)[-1].lower() if "." in f else ""
                if ext in EXTS:
                    p = Path(dirpath) / f
                    try:
                        key = (f, p.stat().st_size)  # dedupe same-name+size
                    except OSError:
                        continue
                    seen.setdefault(key, p)
    return sorted(seen.values())


def run_one(path, timeout=TIMEOUT):
    """Returns (status, seconds): status in ok|error|panic|timeout."""
    t0 = time.time()
    try:
        r = subprocess.run(
            [str(BIN), str(path), "-f", "json"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return "timeout", time.time() - t0
    dt = time.time() - t0
    if r.returncode == 0:
        return "ok", dt
    # Rust panics exit 101; signals are negative returncodes (SIGSEGV/SIGABRT).
    if r.returncode == 101 or r.returncode < 0 or b"panicked" in r.stderr:
        return "panic", dt
    return "error", dt


def mutate(data, seed):
    """Deterministic mutation: truncate or flip a handful of bytes."""
    h = hashlib.sha256(seed.encode()).digest()
    mode = h[0] % 3
    buf = bytearray(data)
    if mode == 0 and len(buf) > 16:  # truncate
        cut = 1 + int.from_bytes(h[1:5], "big") % (len(buf) - 1)
        return bytes(buf[:cut])
    n_flips = 1 + h[1] % 8
    for i in range(n_flips):
        off = int.from_bytes(h[4 * i + 2 : 4 * i + 6], "big") % len(buf)
        buf[off] ^= h[(i + 7) % 32] or 0xFF
    return bytes(buf)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mutations", type=int, default=10, help="mutants per seed file")
    args = ap.parse_args()

    if not BIN.exists():
        sys.exit("build target/release/docparse first")
    corpus = find_corpus()

    # --- clean sweep -------------------------------------------------------
    by_ext = {}
    results = {"ok": 0, "error": 0, "panic": 0, "timeout": 0}
    failures = []
    t_total = 0.0
    for p in corpus:
        status, dt = run_one(p)
        t_total += dt
        results[status] += 1
        ext = p.suffix.lower().lstrip(".")
        d = by_ext.setdefault(ext, {"ok": 0, "error": 0, "panic": 0, "timeout": 0, "t": 0.0})
        d[status] += 1
        d["t"] += dt
        if status in ("panic", "timeout"):
            failures.append((status, str(p)))

    print(f"## 清洁语料扫:{len(corpus)} 份,{t_total:.1f}s 总耗时\n")
    print("| 格式 | 份数 | ok | error | panic | timeout | 耗时 |")
    print("|---|---|---|---|---|---|---|")
    for ext in sorted(by_ext):
        d = by_ext[ext]
        n = d["ok"] + d["error"] + d["panic"] + d["timeout"]
        print(f"| {ext} | {n} | {d['ok']} | {d['error']} | {d['panic']} | {d['timeout']} | {d['t']:.1f}s |")
    print()

    # --- mutation pass -----------------------------------------------------
    pdfs = [p for p in corpus if p.suffix.lower() == ".pdf" and p.stat().st_size < 20_000_000]
    seeds = pdfs[:: max(1, len(pdfs) // 30)][:30]
    mut_results = {"ok": 0, "error": 0, "panic": 0, "timeout": 0}
    mut_failures = []
    with tempfile.TemporaryDirectory() as td:
        for p in seeds:
            data = p.read_bytes()
            for i in range(args.mutations):
                m = mutate(data, f"{p.name}:{i}")
                mp = Path(td) / f"mut-{i}-{p.name}"
                mp.write_bytes(m)
                status, _ = run_one(mp, timeout=60)
                mut_results[status] += 1
                if status in ("panic", "timeout"):
                    mut_failures.append((status, f"{p} mutant {i}"))

    n_mut = sum(mut_results.values())
    print(f"## 变异样本({len(seeds)} 种子 × {args.mutations} 变体 = {n_mut}):", mut_results, "\n")

    for kind, items in (("清洁语料", failures), ("变异样本", mut_failures)):
        if items:
            print(f"### {kind}失败明细")
            for s, p in items:
                print(f"- {s}: {p}")
            print()

    bad = results["panic"] + results["timeout"] + mut_results["panic"] + mut_results["timeout"]
    print(f"**验收门(零 panic/超时):{'通过 ✅' if bad == 0 else f'未过 ❌({bad})'}**")
    sys.exit(0 if bad == 0 else 1)


if __name__ == "__main__":
    main()
