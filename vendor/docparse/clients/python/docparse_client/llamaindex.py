"""LlamaIndex Reader over the docparse client.

Usage::

    from docparse_client.llamaindex import DocparseReader

    nodes = DocparseReader().load_data("paper.pdf")

Each docparse chunk becomes one ``llama_index.core.Document`` whose metadata
carries ``page`` + ``bbox`` + ``heading_path`` for citation back to the
source region. ``llama-index-core`` is imported lazily; install with
``pip install docparse-client[llamaindex]``.
"""

from __future__ import annotations

from pathlib import Path
from typing import List, Union

from . import DocparseClient, DocparseHttpClient


class DocparseReader:
    """LlamaIndex-compatible reader (duck-typed BaseReader: load_data)."""

    def __init__(
        self,
        binary: Union[str, None] = None,
        url: Union[str, None] = None,
        ocr: bool = False,
    ):
        self.ocr = ocr
        self.client = DocparseHttpClient(url) if url else DocparseClient(binary)

    def load_data(self, file_path: Union[str, Path]) -> List["Document"]:  # noqa: F821
        try:
            from llama_index.core import Document
        except ImportError as e:
            raise ImportError(
                "llama-index-core is required: pip install docparse-client[llamaindex]"
            ) from e
        out = []
        for chunk in self.client.chunks(str(file_path), ocr=self.ocr):
            out.append(
                Document(
                    text=chunk["text"],
                    metadata={
                        "source": str(file_path),
                        "page": chunk["page"],
                        "bbox": chunk["bbox"],
                        "heading_path": chunk.get("heading_path", []),
                        "kind": chunk.get("kind", "paragraph"),
                    },
                )
            )
        return out
