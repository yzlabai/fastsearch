"""fastsearch client for qdrant/vector-db-benchmark.

Plug fastsearch's vector backends (brute / HNSW+u8-quant / pgvector-direct) into the
Qdrant benchmark harness via its BaseConfigurator/BaseUploader/BaseSearcher contract.

See README.md for the two prerequisites that must be wired up before a run produces
meaningful numbers (precomputed-vector ingest on /v1/index, and index-param passthrough).
"""

from .configure import FastsearchConfigurator
from .upload import FastsearchUploader
from .search import FastsearchSearcher

__all__ = [
    "FastsearchConfigurator",
    "FastsearchUploader",
    "FastsearchSearcher",
]
