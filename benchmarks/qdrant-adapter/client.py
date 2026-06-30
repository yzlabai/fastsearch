"""Shared connection/config helpers for the fastsearch benchmark client.

fastsearch is a REST service; the three benchmark roles (configure/upload/search) all
talk to the same base URL with an API key. The harness passes `host` plus free-form
`connection_params` / `collection_params` dicts — we read our settings from those.
"""

from __future__ import annotations

import os
from dataclasses import dataclass

# The benchmark uses one logical collection per run; fastsearch keys hits by
# (collection, doc_id, chunk_id). We map the benchmark's integer point id -> chunk_id
# under a single doc, so the search response can be reduced back to that integer id.
BENCH_COLLECTION = os.environ.get("FASTSEARCH_BENCH_COLLECTION", "annbench")
BENCH_DOC_ID = os.environ.get("FASTSEARCH_BENCH_DOC", "bench")


@dataclass
class FastsearchConn:
    base_url: str
    api_key: str
    collection: str = BENCH_COLLECTION
    doc_id: str = BENCH_DOC_ID
    # request timeout (s); upload batches can be large
    timeout: float = 120.0

    @classmethod
    def from_params(cls, host: str, connection_params: dict | None) -> "FastsearchConn":
        p = connection_params or {}
        # host may be a bare hostname (harness convention) or a full URL.
        base = p.get("base_url") or (host if host.startswith("http") else f"http://{host}:8642")
        return cls(
            base_url=base.rstrip("/"),
            api_key=p.get("api_key", os.environ.get("FASTSEARCH_KEY", "dev")),
            collection=p.get("collection", BENCH_COLLECTION),
            doc_id=p.get("doc_id", BENCH_DOC_ID),
            timeout=float(p.get("timeout", 120.0)),
        )

    def headers(self) -> dict:
        return {"X-API-Key": self.api_key, "Content-Type": "application/json"}

    def url(self, path: str) -> str:
        return f"{self.base_url}{path}"
