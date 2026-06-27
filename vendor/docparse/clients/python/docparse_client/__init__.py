"""Thin Python client for docparse (G6 ecosystem integration).

Two transports, zero dependencies:

- :class:`DocparseClient` shells out to the ``docparse`` binary — the simplest
  deployment (one binary + this package);
- :class:`DocparseHttpClient` talks to ``docparse serve`` over REST via
  urllib — for a long-running parser process shared by many callers.

Both return the same shapes: ``parse(..., format="json"|"chunks")`` gives the
decoded JSON (document IR / chunk list), ``"markdown"``/``"text"`` give a str.
Framework adapters live in :mod:`docparse_client.langchain` and
:mod:`docparse_client.llamaindex` (hosts imported lazily).
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import urllib.request
import uuid
from pathlib import Path
from typing import Any, Union

__all__ = ["DocparseClient", "DocparseHttpClient", "DocparseError"]

_TEXT_FORMATS = ("markdown", "text")
_JSON_FORMATS = ("json", "chunks")


class DocparseError(RuntimeError):
    """The parser refused the input or the transport failed."""


def _find_binary(explicit: Union[str, None]) -> str:
    """Resolve the docparse binary: explicit arg > $DOCPARSE_BIN > $PATH."""
    for cand in (explicit, os.environ.get("DOCPARSE_BIN"), shutil.which("docparse")):
        if cand and Path(cand).exists():
            return str(cand)
    raise DocparseError(
        "docparse binary not found: pass binary=..., set DOCPARSE_BIN, "
        "or put `docparse` on PATH"
    )


class DocparseClient:
    """Subprocess transport around the ``docparse`` CLI."""

    def __init__(self, binary: Union[str, None] = None, timeout: float = 300.0):
        self.binary = _find_binary(binary)
        self.timeout = timeout

    def parse(
        self,
        path: Union[str, Path],
        format: str = "json",
        ocr: bool = False,
        layout: bool = False,
        table_model: Union[str, None] = None,
        formula_model: Union[str, None] = None,
    ) -> Any:
        """Parse one document. JSON formats return decoded objects.

        `table_model` / `formula_model` take a UniRec model directory and
        enable the embedded enhancement passes (PDF only).
        """
        if format not in _TEXT_FORMATS + _JSON_FORMATS:
            raise ValueError(f"unknown format {format!r}")
        cmd = [self.binary, str(path), "-f", format]
        if ocr:
            cmd.append("--ocr")
        if layout:
            cmd.append("--layout")
        if table_model:
            cmd += ["--table-model", str(table_model)]
        if formula_model:
            cmd += ["--formula-model", str(formula_model)]
        proc = subprocess.run(
            cmd, capture_output=True, timeout=self.timeout, check=False
        )
        if proc.returncode != 0:
            raise DocparseError(proc.stderr.decode("utf-8", "replace").strip())
        out = proc.stdout.decode("utf-8")
        return json.loads(out) if format in _JSON_FORMATS else out

    def chunks(self, path: Union[str, Path], ocr: bool = False) -> list:
        """RAG chunks: text + page + bbox + heading breadcrumbs."""
        return self.parse(path, format="chunks", ocr=ocr)


class DocparseHttpClient:
    """REST transport for a running ``docparse serve`` instance."""

    def __init__(self, base_url: str = "http://127.0.0.1:8642", timeout: float = 300.0):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout

    def parse(
        self,
        path: Union[str, Path],
        format: str = "json",
        ocr: bool = False,
        layout: bool = False,
        table_model: bool = False,
        formula_model: bool = False,
    ) -> Any:
        """Boolean enhancement flags map to query params; the server must be
        started with the matching model config (--unirec-models etc.)."""
        if format not in _TEXT_FORMATS + _JSON_FORMATS:
            raise ValueError(f"unknown format {format!r}")
        path = Path(path)
        boundary = uuid.uuid4().hex
        body = b"".join(
            [
                f"--{boundary}\r\n".encode(),
                f'Content-Disposition: form-data; name="file"; filename="{path.name}"\r\n'.encode(),
                b"Content-Type: application/octet-stream\r\n\r\n",
                path.read_bytes(),
                f"\r\n--{boundary}--\r\n".encode(),
            ]
        )
        flags = "".join(
            f"&{k}=true"
            for k, v in [
                ("ocr", ocr),
                ("layout", layout),
                ("table_model", table_model),
                ("formula_model", formula_model),
            ]
            if v
        )
        url = f"{self.base_url}/parse?format={format}{flags}"
        req = urllib.request.Request(
            url,
            data=body,
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                out = resp.read().decode("utf-8")
        except urllib.error.HTTPError as e:  # surface the server's message
            raise DocparseError(e.read().decode("utf-8", "replace")) from e
        except urllib.error.URLError as e:
            raise DocparseError(str(e)) from e
        return json.loads(out) if format in _JSON_FORMATS else out

    def chunks(self, path: Union[str, Path], ocr: bool = False) -> list:
        return self.parse(path, format="chunks", ocr=ocr)
