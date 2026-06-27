# docparse-client

Thin Python client for [docparse](https://github.com/yzlabai/docparse-rs) —
the pure-Rust document parser. Zero runtime dependencies: wraps the
`docparse` binary (subprocess) or a running `docparse serve` (REST/urllib).

```bash
pip install docparse-client          # + put the docparse binary on PATH
```

## Parse

```python
from docparse_client import DocparseClient

client = DocparseClient()                      # finds docparse on PATH / $DOCPARSE_BIN
doc    = client.parse("paper.pdf")             # full IR (dict, provenance + coordinates)
chunks = client.chunks("paper.pdf")            # RAG chunks: text + page + bbox + breadcrumbs
md     = client.parse("paper.pdf", format="markdown")
```

Against a long-running server (`docparse serve --port 8642`):

```python
from docparse_client import DocparseHttpClient
chunks = DocparseHttpClient("http://127.0.0.1:8642").chunks("paper.pdf")
```

## LangChain

```python
from docparse_client.langchain import DocparseLoader

docs = DocparseLoader("paper.pdf").load()
docs[0].metadata   # {"source", "page", "bbox", "heading_path", "kind"}
```

`pip install docparse-client[langchain]`. Every Document carries `page` +
`bbox` — answers cite back to a highlightable region of the source.

## LlamaIndex

```python
from docparse_client.llamaindex import DocparseReader
nodes = DocparseReader().load_data("paper.pdf")
```

`pip install docparse-client[llamaindex]`.

## Tests

```bash
cargo build --release
DOCPARSE_BIN=target/release/docparse python3 -m unittest discover clients/python/tests -v
```
