"""E2E tests against the real binary (and a live `docparse serve`).

Run from the repo root after `cargo build --release`:

    DOCPARSE_BIN=target/release/docparse python3 -m unittest discover clients/python/tests -v

stdlib-only on purpose — the client has no dependencies, neither do its tests.
"""

import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from docparse_client import DocparseClient, DocparseError, DocparseHttpClient  # noqa: E402

BIN = os.environ.get("DOCPARSE_BIN", "target/release/docparse")

SAMPLE_MD = """# Title

A paragraph of body text for the client test.

- alpha
- beta
"""


def _sample(tmp, name="sample.md", content=SAMPLE_MD):
    p = Path(tmp) / name
    p.write_text(content, encoding="utf-8")
    return p


@unittest.skipUnless(Path(BIN).exists(), f"binary not built: {BIN}")
class SubprocessClientTest(unittest.TestCase):
    def setUp(self):
        self.client = DocparseClient(binary=BIN)

    def test_json_and_chunks_and_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = _sample(tmp)
            doc = self.client.parse(p, format="json")
            self.assertEqual(doc["pages"][0]["number"], 1)
            chunks = self.client.chunks(p)
            self.assertTrue(any("body text" in c["text"] for c in chunks))
            self.assertIn("bbox", chunks[0])
            text = self.client.parse(p, format="text")
            self.assertIn("Title", text)

    def test_error_is_clean(self):
        with self.assertRaises(DocparseError):
            self.client.parse("does-not-exist.pdf")

    def test_unknown_format_rejected(self):
        with self.assertRaises(ValueError):
            self.client.parse("x.md", format="yaml")

    def test_langchain_loader_metadata_without_langchain(self):
        # Without langchain-core installed the loader must fail with a clear
        # ImportError — not an AttributeError mid-iteration.
        from docparse_client.langchain import DocparseLoader

        with tempfile.TemporaryDirectory() as tmp:
            p = _sample(tmp)
            loader = DocparseLoader(p, binary=BIN)
            try:
                docs = loader.load()
            except ImportError as e:
                self.assertIn("langchain", str(e))
            else:  # langchain-core present: verify the metadata contract
                self.assertTrue(docs)
                self.assertEqual(docs[0].metadata["page"], 1)
                self.assertIn("bbox", docs[0].metadata)


@unittest.skipUnless(Path(BIN).exists(), f"binary not built: {BIN}")
class HttpClientTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            cls.port = s.getsockname()[1]
        cls.server = subprocess.Popen(
            [BIN, "serve", "--port", str(cls.port)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.time() + 10
        import urllib.request

        while time.time() < deadline:
            try:
                urllib.request.urlopen(
                    f"http://127.0.0.1:{cls.port}/healthz", timeout=1
                )
                return
            except OSError:
                time.sleep(0.2)
        raise RuntimeError("docparse serve did not come up")

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        cls.server.wait(timeout=10)

    def test_rest_chunks_match_subprocess(self):
        http = DocparseHttpClient(f"http://127.0.0.1:{self.port}")
        sub = DocparseClient(binary=BIN)
        with tempfile.TemporaryDirectory() as tmp:
            p = _sample(tmp)
            self.assertEqual(http.chunks(p), sub.chunks(p))
            md = http.parse(p, format="markdown")
            self.assertIn("# Title", md)


if __name__ == "__main__":
    unittest.main()
