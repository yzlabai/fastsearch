"""FastsearchUploader — batch-upload precomputed vectors via POST /v1/index.

Each benchmark point (integer id + float vector + optional payload) becomes a fastsearch
Chunk under a single benchmark doc, with chunk_id = point id. We send the raw vector on the
chunk so the server skips embedding.

⚠️ PREREQUISITE (README #1): /v1/index currently embeds chunk.text and ignores any supplied
vector. The engine already has Engine::ingest_vector — this uploader assumes a small server
change that routes a chunk carrying `vector` straight to ingest_vector. Without it, numbers
reflect text-embedding of empty text, which is meaningless. Fail loud rather than mislead.
"""

from __future__ import annotations

import requests

from .client import FastsearchConn

try:
    from engine.base_client.upload import BaseUploader
except ImportError:
    BaseUploader = object  # type: ignore


class FastsearchUploader(BaseUploader):
    DEFAULT_BATCH = 256

    def __init__(self, host, connection_params, upload_params):
        super().__init__(host, connection_params, upload_params)
        self.conn = FastsearchConn.from_params(host, connection_params)
        self.upload_params = upload_params or {}

    @classmethod
    def init_client(cls, host, distance, connection_params, upload_params):
        return cls(host, connection_params, upload_params)

    def _chunk(self, point_id: int, vector: list[float], payload: dict | None) -> dict:
        # Chunk schema (core::Chunk). text="" => not indexed for BM25; this is a pure vector run.
        # Filterable payload fields (for the filtered-search scenario) ride in `meta`/acl as the
        # engine expects; keep them minimal until the filter mapping is finalized.
        return {
            "doc_id": self.conn.doc_id,
            "chunk_id": int(point_id),
            "kind": "paragraph",  # core::ChunkKind, snake_case
            "text": "",            # empty => not indexed for BM25; pure vector run
            "page": 0,
            "bbox": {"x0": 0.0, "y0": 0.0, "x1": 0.0, "y1": 0.0},
            "char_len": 0,
            # benchmark bypass: raw vector consumed directly by ingest_vector (no embedding).
            "vector": list(vector),
            **({"payload": payload} if payload else {}),
        }

    def upload_batch(self, ids: list[int], vectors, payloads: list[dict] | None):
        payloads = payloads or [None] * len(ids)
        chunks = [self._chunk(i, v, p) for i, v, p in zip(ids, vectors, payloads)]
        body = {
            "collection": self.conn.collection,
            "doc_id": self.conn.doc_id,
            "chunks": chunks,
        }
        resp = requests.post(
            self.conn.url("/v1/index"),
            json=body,
            headers=self.conn.headers(),
            timeout=self.conn.timeout,
        )
        resp.raise_for_status()
        indexed = resp.json().get("indexed", 0)
        if indexed != len(chunks):
            raise RuntimeError(
                f"/v1/index returned indexed={indexed}, expected {len(chunks)}; "
                "is the precomputed-vector ingest path wired up? (README prerequisite #1)"
            )

    # NOTE: doc-level replace semantics mean every upload_batch with the same doc_id REPLACES
    # the doc. The harness uploads one batch-stream per collection; if it sends multiple
    # batches, accumulate them into one /v1/index call OR use distinct doc_ids per batch and
    # reduce at search time. TODO: confirm the harness's batching and adjust (see README).

    def finalize(self):
        # Force a commit / wait for index build so search reflects all points.
        # fastsearch commits inside /v1/index; if an async build is added, block here.
        return
