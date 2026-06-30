"""FastsearchSearcher — single-query vector search via POST /v1/search.

Sends mode="vector" with the raw query vector (no server-side embedding), returns the
benchmark point ids in rank order. The harness times these calls to build the
recall-vs-QPS frontier, so keep the body minimal and reuse a session for connection reuse.
"""

from __future__ import annotations

import requests

from .client import FastsearchConn

try:
    from engine.base_client.search import BaseSearcher
except ImportError:
    BaseSearcher = object  # type: ignore


class FastsearchSearcher(BaseSearcher):
    # One pooled session per process; the harness may run many query threads.
    _session: requests.Session | None = None

    def __init__(self, host, connection_params, search_params):
        super().__init__(host, connection_params, search_params)
        self.conn = FastsearchConn.from_params(host, connection_params)
        self.search_params = search_params or {}

    @classmethod
    def init_client(cls, host, distance, connection_params, search_params):
        inst = cls(host, connection_params, search_params)
        cls._session = requests.Session()
        return inst

    def search_one(self, vector: list[float], meta_conditions, top: int):
        # meta_conditions -> core::Filter AST for the filtered-search scenario.
        # The harness's filter representation must be translated to fastsearch's Filter;
        # left as TODO until the filtered benchmark is run (README). None = unfiltered.
        body = {
            "query": "",            # pure vector; no lexical side
            "mode": "vector",
            "vector": list(vector),
            "top_k": top,
            # over-fetch candidates a bit above top_k so HNSW recall isn't truncated.
            "candidates": max(top, int(self.search_params.get("candidates", top * 4))),
        }
        # Per-query HNSW ef_search override — THE knob to sweep for the recall-vs-QPS curve.
        # Set search_params["ef_search"] per experiment point (no reindex, no restart).
        # Ignored by brute/pgvector backends. None => server's configured default.
        ef = self.search_params.get("ef_search")
        if ef is not None:
            body["ef_search"] = int(ef)
        if meta_conditions:
            body["filter"] = self._translate_filter(meta_conditions)  # TODO

        sess = self._session or requests
        resp = sess.post(
            self.conn.url("/v1/search"),
            json=body,
            headers=self.conn.headers(),
            timeout=self.conn.timeout,
        )
        resp.raise_for_status()
        hits = resp.json().get("hits", [])
        # Reduce hits back to (id, score). chunk_id was set to the benchmark point id at upload.
        out = []
        for h in hits:
            cid = h.get("chunk_id")
            if cid is None:
                # hits may expose the id only via citation_id "collection:doc_id:chunk_id"
                cid = self._chunk_id_from_citation(h.get("citation_id", ""))
            score = h.get("score", 0.0)
            out.append((int(cid), float(score)))
        return out

    @staticmethod
    def _chunk_id_from_citation(citation_id: str) -> int:
        # "{collection}:{doc_id}:{chunk_id}" — chunk_id is the last segment (doc_id may contain ':').
        return int(citation_id.rsplit(":", 1)[-1]) if citation_id else -1

    def _translate_filter(self, meta_conditions):
        # TODO: map the harness's filter dict to core::Filter AST (eq/range on payload fields).
        raise NotImplementedError("filtered-search translation not wired up yet (README)")
