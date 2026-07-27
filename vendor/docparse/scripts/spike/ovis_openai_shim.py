#!/usr/bin/env python3
"""Serve a local HF vision-language model behind the OpenAI chat-completions API.

Why this exists: the shipped path (`--table-vlm`, `--transcribe-vlm`,
fastsearch's `parse-vlm`) speaks HTTP to an OpenAI-compatible endpoint. On a
machine without CUDA there is no vLLM, so evaluating the *product* — not just
the model — needs a shim. With it, the entire Rust pipeline and the OmniDocBench
harness run unchanged against a real model.

That distinction matters: calling the model directly from Python measures the
model. Going through this shim measures what we ship — the same crop geometry,
prompt, `max_tokens`, HTML parsing and degradation gates. Two earlier attempts
to shortcut it produced numbers that had to be thrown away.

    pip install torch torchvision transformers accelerate pillow
    python3 scripts/spike/ovis_openai_shim.py --model ATH-MaaS/OvisOCR2 --port 8000

    # then, unchanged:
    docparse page.pdf --table-vlm --vlm-url http://127.0.0.1:8000 --vlm-model ovis

Deliberately minimal: one request at a time (a Mac has one GPU anyway), no
batching, no streaming. `--device` defaults to mps, falls back to cpu.
"""
import argparse
import base64
import io
import json
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {}


def load(model_id, device, dtype):
    import torch
    from transformers import AutoProcessor, AutoModelForImageTextToText
    t = time.time()
    proc = AutoProcessor.from_pretrained(model_id)
    dt = {"bf16": torch.bfloat16, "fp16": torch.float16, "fp32": torch.float32}[dtype]
    model = AutoModelForImageTextToText.from_pretrained(model_id, dtype=dt, device_map=device)
    model.eval()
    print(f"loaded {model_id} on {device}/{dtype} in {time.time() - t:.1f}s", flush=True)
    return proc, model


def decode_image(url):
    """data: URL or bare base64 → PIL image."""
    from PIL import Image
    b64 = re.sub(r"^data:image/[^;]+;base64,", "", url)
    return Image.open(io.BytesIO(base64.b64decode(b64))).convert("RGB")


def generate(messages, max_tokens, temperature):
    import torch
    proc, model = STATE["proc"], STATE["model"]
    # Rewrite OpenAI content parts into the processor's chat format.
    conv = []
    for m in messages:
        parts = m.get("content")
        if isinstance(parts, str):
            conv.append({"role": m["role"], "content": [{"type": "text", "text": parts}]})
            continue
        out = []
        for p in parts:
            if p.get("type") == "image_url":
                out.append({"type": "image", "image": decode_image(p["image_url"]["url"])})
            elif p.get("type") == "text":
                out.append({"type": "text", "text": p["text"]})
        conv.append({"role": m["role"], "content": out})

    inputs = proc.apply_chat_template(
        conv, add_generation_prompt=True, tokenize=True,
        return_dict=True, return_tensors="pt").to(model.device)
    kw = dict(max_new_tokens=max_tokens, do_sample=temperature > 0)
    if temperature > 0:
        kw["temperature"] = temperature
    with torch.no_grad():
        out = model.generate(**inputs, **kw)
    gen = out[0][inputs["input_ids"].shape[1]:]
    return proc.decode(gen, skip_special_tokens=True).strip(), int(gen.shape[0])


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # /v1/models — clients (and our own harness preflight) probe this.
        if self.path.rstrip("/").endswith("/v1/models"):
            self._send(200, {"object": "list", "data": [
                {"id": STATE["name"], "object": "model", "owned_by": "local"}]})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if not self.path.rstrip("/").endswith("/v1/chat/completions"):
            return self._send(404, {"error": "not found"})
        try:
            req = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
            t = time.time()
            text, ntok = generate(req.get("messages", []),
                                  int(req.get("max_tokens") or 1024),
                                  float(req.get("temperature") or 0.0))
            dt = time.time() - t
            print(f"  {ntok:4d} tok in {dt:5.1f}s ({ntok / max(dt, 1e-6):.1f} tok/s)", flush=True)
            self._send(200, {
                "object": "chat.completion",
                "model": req.get("model", STATE["name"]),
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": text}}],
                "usage": {"completion_tokens": ntok},
            })
        except Exception as e:  # a shim failure must look like a service error,
            print(f"  ! {type(e).__name__}: {e}", flush=True)  # not a crash — the
            self._send(500, {"error": str(e)})  # caller's job is to degrade.

    def log_message(self, *a):
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="ATH-MaaS/OvisOCR2")
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--device", default="mps")
    ap.add_argument("--dtype", default="bf16", choices=["bf16", "fp16", "fp32"])
    a = ap.parse_args()

    device = a.device
    try:
        import torch
        if device == "mps" and not torch.backends.mps.is_available():
            print("mps unavailable → cpu")
            device = "cpu"
    except ImportError:
        raise SystemExit("torch not installed: pip install torch torchvision transformers accelerate pillow")

    STATE["proc"], STATE["model"] = load(a.model, device, a.dtype)
    STATE["name"] = a.model.split("/")[-1]
    srv = ThreadingHTTPServer(("127.0.0.1", a.port), Handler)
    print(f"listening on http://127.0.0.1:{a.port}  (model name: {STATE['name']})", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()


if __name__ == "__main__":
    main()
