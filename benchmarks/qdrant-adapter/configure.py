"""FastsearchConfigurator — register the benchmark collection and pin the backend-under-test.

fastsearch selects its vector backend (brute / brute_binary / hnsw / pgvector) and HNSW
params at the *server* level via FASTSEARCH_VECTOR_BACKEND env — NOT per collection. So the
correct benchmark pattern (same as ann-benchmarks/Qdrant: one process per engine config) is:

  - launch one fastsearch server per backend variant you want to compare, and
  - this configurator REGISTERS the collection (dim/distance) via POST /v1/collections and
    ASSERTS the running server's actual backend matches what the experiment claims to test.

It does not silently pretend a per-collection backend took effect — it reads the truth back
from the server and fails loud on mismatch.
"""

from __future__ import annotations

import requests

from .client import FastsearchConn

try:
    # Provided by the qdrant/vector-db-benchmark package once this lives under engine/clients/.
    from engine.base_client.configure import BaseConfigurator
    from engine.base_client.distances import Distance
except ImportError:  # standalone lint / editor outside the harness checkout
    BaseConfigurator = object  # type: ignore
    Distance = None  # type: ignore

# fastsearch vector backends, as the server reports them in /v1/collections .server.vector_backend.
# Selected at server startup via FASTSEARCH_VECTOR_BACKEND (relaunch to switch); keep in sync.
SUPPORTED_BACKENDS = {"brute", "brute_binary", "brute_binary_rotated", "hnsw", "pgvector"}

# Map the harness distance enum to what fastsearch expects. fastsearch's MemVectorIndex is
# cosine; angular datasets (glove) are cosine-equivalent after normalization. Euclidean
# (sift/gist) requires the backend to support L2 — flag loudly if it doesn't yet.
_DISTANCE_NAMES = {"cosine", "dot", "l2"}


class FastsearchConfigurator(BaseConfigurator):
    def __init__(self, host, collection_params: dict, connection_params: dict):
        super().__init__(host, collection_params, connection_params)
        self.conn = FastsearchConn.from_params(host, connection_params)
        self.collection_params = collection_params or {}

    def clean(self):
        """Reset state for a fresh run: drop the benchmark doc so re-uploads don't accrete.

        fastsearch replaces a doc on re-index (remove_doc + ingest), so a fresh /v1/index
        with the same doc_id already replaces. A dedicated delete endpoint is cleaner; until
        one exists, the uploader's first batch (full doc replace) is the reset point.
        """
        # TODO: call a server "drop collection" admin endpoint once exposed.
        return

    def recreate(self, dataset, collection_params):
        """Pin backend + distance + HNSW/quant params for this run.

        `dataset.config` carries vector_size and distance. We resolve the fastsearch backend
        from collection_params["fastsearch_backend"] (default: brute, the deterministic one).
        """
        backend = (collection_params or {}).get("fastsearch_backend", "brute")
        if backend not in SUPPORTED_BACKENDS:
            raise ValueError(
                f"unknown fastsearch_backend={backend!r}; expected one of {sorted(SUPPORTED_BACKENDS)}"
            )

        distance = getattr(dataset.config, "distance", None)
        dim = getattr(dataset.config, "vector_size", None)

        # Distance sanity: brute/hnsw are cosine today; reject l2 datasets until L2 lands.
        dname = str(distance).lower()
        if "euclid" in dname or dname.endswith("l2"):
            raise NotImplementedError(
                "euclidean datasets (sift/gist) need an L2-capable backend; "
                "start with angular/cosine datasets (glove, dbpedia-openai) first"
            )

        # Register the collection (advisory dim/distance) and read back the server's actual
        # vector backend — the source of truth for "what is under test".
        resp = requests.post(
            self.conn.url("/v1/collections"),
            json={"name": self.conn.collection, "dim": dim, "distance": "cosine"},
            headers=self.conn.headers(),
            timeout=self.conn.timeout,
        )
        resp.raise_for_status()
        server_backend = resp.json().get("server", {}).get("vector_backend")
        print(
            f"[fastsearch] collection={self.conn.collection} dim={dim} "
            f"requested_backend={backend} server_backend={server_backend}"
        )
        # Fail loud if the running server isn't the backend this experiment claims to test.
        # (Backend is chosen at server startup via FASTSEARCH_VECTOR_BACKEND — relaunch to switch.)
        if server_backend != backend:
            raise RuntimeError(
                f"server backend is {server_backend!r} but experiment requested {backend!r}; "
                f"relaunch the server with FASTSEARCH_VECTOR_BACKEND={backend} "
                "(per-collection backend selection is intentionally not supported)"
            )
