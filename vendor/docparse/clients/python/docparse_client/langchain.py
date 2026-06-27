"""LangChain DocumentLoader over the docparse client.

Five lines to load a PDF with citation metadata::

    from docparse_client.langchain import DocparseLoader

    docs = DocparseLoader("paper.pdf").load()
    docs[0].page_content   # chunk text
    docs[0].metadata       # {"source", "page", "bbox", "heading_path", "kind"}

Each docparse chunk becomes one LangChain ``Document``; ``page`` + ``bbox``
make every answer traceable back to a highlightable region of the source —
the metadata RAG citations need and most loaders drop.

``langchain-core`` is imported lazily so the base package stays
dependency-free; install with ``pip install docparse-client[langchain]``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterator, Union

from . import DocparseClient, DocparseHttpClient


class DocparseLoader:
    """LangChain-compatible loader (duck-typed BaseLoader: load/lazy_load)."""

    def __init__(
        self,
        file_path: Union[str, Path],
        binary: Union[str, None] = None,
        url: Union[str, None] = None,
        ocr: bool = False,
    ):
        self.file_path = str(file_path)
        self.ocr = ocr
        self.client = DocparseHttpClient(url) if url else DocparseClient(binary)

    def lazy_load(self) -> Iterator["Document"]:  # noqa: F821 (lazy import)
        try:
            from langchain_core.documents import Document
        except ImportError as e:
            raise ImportError(
                "langchain-core is required: pip install docparse-client[langchain]"
            ) from e
        for chunk in self.client.chunks(self.file_path, ocr=self.ocr):
            yield Document(
                page_content=chunk["text"],
                metadata={
                    "source": self.file_path,
                    "page": chunk["page"],
                    "bbox": chunk["bbox"],
                    "heading_path": chunk.get("heading_path", []),
                    "kind": chunk.get("kind", "paragraph"),
                },
            )

    def load(self) -> list:
        return list(self.lazy_load())
