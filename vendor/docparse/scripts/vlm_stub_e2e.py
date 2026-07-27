#!/usr/bin/env python3
"""End-to-end smoke test for the VLM region path — no GPU, no models, no network.

The unit tests cover the pieces (protocol shape, batch ordering, HTML grid
parsing, degradation gates). What none of them cover is the *wiring*: CLI flag →
`VlmRegionReader` → `refine_tables` → cropped render → answer → `Table` in the
IR. This script closes that gap by standing up a stub OpenAI-compatible endpoint
and a synthetic PDF holding one ruled table, then asserting on what comes out.

It also pins the request shape the service actually receives, which is where two
real bugs already hid: `max_tokens`/`temperature` were not being sent at all, and
the image cap was hardcoded to the captioning default.

    cargo build            # or --release; pass the binary path as argv[1]
    python3 scripts/vlm_stub_e2e.py [path/to/docparse]

Exits non-zero on the first failed assertion. Stdlib only.
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/debug/docparse")

# Merged cells on purpose: rowspan/colspan is exactly the topology the HTML
# prompt buys over the TSV one, so the assertion below is the reason this
# backend exists.
ANSWER = (
    '<table><tr><td rowspan="2">Anno</td><td colspan="2">Ricavi</td></tr>'
    "<tr><td>2024</td><td>2025</td></tr>"
    "<tr><td>Totale</td><td>1.234</td><td>5.678</td></tr></table>"
)


def make_table_pdf(path):
    """A minimal PDF with one ruled 2x2 table — enough for the deterministic
    lattice detector to fire, so the VLM pass has something to refine."""
    rows, cols = [700, 660, 620], [100, 250, 400]
    ops = []
    for y in rows:  # thin filled rects: how real PDFs draw table rules
        ops.append(f"{cols[0]} {y - 0.5} {cols[-1] - cols[0]} 1 re f")
    for x in cols:
        ops.append(f"{x - 0.5} {rows[-1]} 1 {rows[0] - rows[-1]} re f")
    for t, x, y in [("Anno", 110, 670), ("Ricavi", 260, 670),
                    ("2025", 110, 630), ("42", 260, 630)]:
        ops.append(f"BT /F1 12 Tf {x} {y} Td ({t}) Tj ET")
    content = "\n".join(ops).encode()

    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length " + str(len(content)).encode() + b" >>\nstream\n" + content + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    out, offsets = bytearray(b"%PDF-1.4\n"), []
    for i, body in enumerate(objs, 1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objs) + 1}\n".encode() + b"0000000000 65535 f \n"
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += (f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R >>\n"
            f"startxref\n{xref}\n").encode() + b"%%EOF\n"
    open(path, "wb").write(bytes(out))


class Stub(BaseHTTPRequestHandler):
    seen = []

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
        img = body["messages"][0]["content"][1]["image_url"]["url"]
        Stub.seen.append({
            "path": self.path,
            "model": body.get("model"),
            "max_tokens": body.get("max_tokens"),
            "temperature": body.get("temperature"),
            "image_prefix": img[:22],
            "prompt": body["messages"][0]["content"][0]["text"],
        })
        out = json.dumps({"choices": [{"message": {"content": ANSWER}}]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *a):
        pass


def check(cond, what):
    print(f"  {'✓' if cond else '✗'} {what}")
    if not cond:
        sys.exit(1)


def main():
    if not os.path.exists(BIN):
        sys.exit(f"binary not found: {BIN} (cargo build, or pass the path)")

    srv = HTTPServer(("127.0.0.1", 0), Stub)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{srv.server_address[1]}"

    with tempfile.TemporaryDirectory() as d:
        pdf = os.path.join(d, "table.pdf")
        make_table_pdf(pdf)

        base = json.loads(subprocess.run(
            [BIN, pdf, "-f", "json"], capture_output=True, text=True, check=True).stdout)
        tables = [e for p in base["pages"] for e in p["elements"] if e["type"] == "table"]
        print("deterministic baseline")
        check(len(tables) == 1, "one table detected without any model")
        check(tables[0].get("source") is None, "deterministic table carries no source tag")

        got = json.loads(subprocess.run(
            [BIN, pdf, "-f", "json", "--table-vlm", "--vlm-url", url,
             "--vlm-model", "stub-model"],
            capture_output=True, text=True, check=True).stdout)
        t = [e for p in got["pages"] for e in p["elements"] if e["type"] == "table"][0]
        cells = [[c["text"] for c in r] for r in t["rows"]]

        print("after --table-vlm")
        check(t.get("source") == "table:vlm:stub-model", f"source tag: {t.get('source')}")
        check(cells == [["Anno", "Ricavi", "Ricavi"],
                        ["Anno", "2024", "2025"],
                        ["Totale", "1.234", "5.678"]],
              f"rowspan/colspan expanded into every covered cell: {cells}")
        check(t["bbox"] == tables[0]["bbox"],
              "bbox stays the detected geometry (the model never invents coordinates)")

        print("request the service actually received")
        req = Stub.seen[0]
        check(len(Stub.seen) == 1, "exactly one call for one table")
        check(req["path"] == "/v1/chat/completions", "OpenAI-compatible path")
        check(req["model"] == "stub-model", "model name passed through")
        check(req["max_tokens"] == 2000, f"max_tokens sent: {req['max_tokens']}")
        check(req["temperature"] == 0.0, f"temperature pinned: {req['temperature']}")
        check(req["image_prefix"] == "data:image/png;base64,", "PNG data-URL image")
        check("<table>" in req["prompt"], "HTML prompt (not the TSV one)")

        print("service failure must degrade, never fail the parse")
        # `shutdown()` alone only stops the serve loop — the listening socket
        # stays open, so connections are accepted and then black-holed until the
        # client's 120s timeout. `server_close()` makes the port refuse, which
        # is the failure mode worth asserting on here (a *hanging* service is a
        # different, slower story; see the timeout note in the spec).
        srv.shutdown()
        srv.server_close()
        r = subprocess.run(
            [BIN, pdf, "-f", "json", "--table-vlm", "--vlm-url", url,
             "--vlm-model", "stub-model"], capture_output=True, text=True)
        check(r.returncode == 0, "exit code 0 with the service down")
        down = json.loads(r.stdout)
        t2 = [e for p in down["pages"] for e in p["elements"] if e["type"] == "table"][0]
        check([[c["text"] for c in r_] for r_ in t2["rows"]]
              == [[c["text"] for c in r_] for r_ in tables[0]["rows"]],
              "deterministic rows survive intact")
        check(t2.get("source") is None, "no source tag claimed for work that didn't happen")

    print("\nOK — VLM region wiring verified end to end (no GPU, no models).")


if __name__ == "__main__":
    main()
