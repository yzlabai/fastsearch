# Using fastsearch in Agent development

> 🌏 中文版: [在Agent中使用fastsearch.md](在Agent中使用fastsearch.md)

> A developer-facing guide to using fastsearch as the **retrieval and grounding layer for AI Agents / RAG**.
> It draws on how peers (Meilisearch, Qdrant, pgvector, Exa/Tavily, …) approach agentic-RAG
> (see "Comparison with alternatives" at the end), grounded in fastsearch's **real API** (every example below runs as-is).

---

## 0. One-liner and positioning

**fastsearch = a hybrid-search engine (keyword + vector), with managed Postgres as the source of truth, built natively for "traceable, multi-tenant" Agent retrieval.**

Why pick it for Agent development (vs. a pure vector store / pure keyword index):

| Capability | What it means for an Agent |
|---|---|
| **Traceable citations** (`citation_id → page+bbox`, `resolve_citation` deep links) | The LLM's answer can **carry its sources** and link back to the original page/coordinates — grounding isn't just "found the passage" but "points to the location" |
| **Hybrid retrieval** (keyword ∥ vector → RRF fusion) | The recall layer for agentic-RAG: exact lexical + semantic understanding — recall accurate candidates first, then run a correction/verification loop |
| **ACL cannot be bypassed** (injected server-side per API key) | Multi-tenant Agents are isolated by construction — the client/LLM **cannot** pass or widen permissions in the request |
| **MCP-native** (the fourth face) | The LLM calls `search` / `resolve_citation` directly as tools — no HTTP glue to write |
| **Filter-aware** | Highly selective filters **don't lose recall** (avoiding pgvector's post-filter trap) — well-suited to corrective retrieval |
| **Highlight snippets** | Return only the matched snippet, **saving tokens** (akin to Exa highlights) |
| **PG source of truth, no lock-in** | Your data lives in your own managed PG (RDS/Supabase/Neon); the engine index is rebuildable — no proprietary storage |

---

## 1. Five-minute quickstart (a closed loop with zero local deps)

No PG, no model needed — feed it a **folder** and search:

```bash
cargo build -p fastsearch-cli --bin fastsearch

# ① Feed a folder (recurses .md/.txt; markdown headings auto-become breadcrumbs)
./target/debug/fastsearch index-dir --data ./idx --collection kb  ./my-docs

# ② Search (results carry page + heading_path provenance; --json for structured output)
./target/debug/fastsearch search --data ./idx --collection kb --query "gross margin" --json
```

Output (each hit carries a traceable citation):

```json
[{ "citation_id": "kb:reports/2024-annual.md:4", "score": 0.0164,
   "doc_id": "reports/2024-annual.md", "page": 1,
   "heading_path": ["2024 Annual Financial Report", "Risk Disclosure"] }]
```

> This is a **purely local, no-external-dependency** end-to-end retrieval loop — ideal for validating retrieval quality before deciding to wire up PG / vectors / an Agent.

---

## 2. Core concepts for Agent development

### 2.1 Citations and grounding (the most important)

Every hit carries **`citation_id` = `collection:doc_id:chunk_id`** + `page` + `bbox` + `heading_path`.
Have the LLM cite `citation_id` in its answer, then use **`resolve_citation`** to resolve it into a deep-linkable source location:

```
search → feed {citation_id, snippet} into the prompt → LLM generates an answer with [citation_id]
       → resolve_citation(citation_id) → {page, bbox} → frontend highlights / jumps to source
```

REST: `GET /v1/asset/{citation_id}` → `DocRender{page,bbox}` / a signed media URL (ACL enforced; unauthorized/nonexistent both return 404).

### 2.2 Three search modes

- `keyword`: BM25 full-text — deterministic, sub-millisecond, no model needed. **The default starting point.**
- `vector`: semantic nearest-neighbor (requires an embedding backend, see §7). Recalls even when terms don't overlap.
- `hybrid`: keyword ∥ vector → RRF fusion. **Recommended for agentic-RAG** (precise + semantic).

### 2.3 Filtering (filter-aware, no recall loss)

Filters are translated into a **superset index query + exact post-filter** — highly selective filters lose no recall. Filter AST (JSON, snake_case outer tags):

```json
{"and": [
  {"eq": ["modality", "image"]},
  {"gte": ["page", 5]},
  {"in": ["kind", ["table", "paragraph"]]}
]}
```

Operators: `and/or/not · eq/ne/gt/gte/lt/lte · in · exists · heading_prefix`.
Filterable fields: `kind / modality / doc_id / collection / page / section_id / tenant` (time lives in media).

### 2.4 Highlighting (save tokens)

`"highlight": true` → hits carry a `highlight` snippet (matched terms wrapped in `<b>`). Feed only the snippet to the LLM to save context.

---

## 3. Four ways to integrate (the four faces)

### 3.1 MCP — Agent-native (recommended for LLM agents)

Expose retrieval as **MCP tools** to any MCP client (Claude Desktop / IDE / your own host). No HTTP glue to write.

```bash
cargo build -p fastsearch-mcp --bin fastsearch-mcp
```

MCP client config (stdio server):

```json
{
  "mcpServers": {
    "fastsearch": {
      "command": "/abs/path/target/debug/fastsearch-mcp",
      "env": {
        "FASTSEARCH_DATA": "/abs/path/idx",
        "FASTSEARCH_TOKENIZER": "jieba",
        "FASTSEARCH_MCP_TENANT": "acme",
        "FASTSEARCH_MCP_TAGS": "team-a,public"
      }
    }
  }
}
```

It exposes two tools:

- **`search`**: input `{query, mode?, top_k?, filter?, highlight?}` → hits with citations (citation_id/page/heading_path/snippet).
- **`resolve_citation`**: input `{citation_id}` → media/source location (page+bbox or signed URL).

ACL is injected from server-side env (`FASTSEARCH_MCP_TENANT/TAGS`) — the LLM's tool arguments **cannot** smuggle in or widen permissions.

### 3.2 REST — any Agent framework

```bash
FASTSEARCH_DATA=./idx FASTSEARCH_KEYS="dev=acme:team-a,public" \
  cargo run -p fastsearch-server --bin fastsearch-server   # :8642
```

```bash
# Search (hybrid + filter + highlight + deep-pagination cursor)
curl -s localhost:8642/v1/search -H "x-api-key: dev" -H "content-type: application/json" -d '{
  "query": "why did gross margin decline", "mode": "hybrid", "top_k": 8, "highlight": true,
  "filter": {"eq": ["kind", "table"]}
}'
# → { "hits": [ { "citation_id", "score", "page", "bbox", "heading_path",
#                 "highlight", "cursor", "time", "media" } ], "facets": {} }

# Similar (more_like_this, looked up by citation_id)
curl -s localhost:8642/v1/search ... ; curl -s localhost:8642/v1/similar -H "x-api-key: dev" \
  -d '{"citation_id":"kb:rep.pdf:3","top_k":5}'

# Media / source-location gateway (ACL enforced; unauthorized/nonexistent → 404)
curl -s localhost:8642/v1/asset/kb:rep.pdf:3 -H "x-api-key: dev"

# Ingest (doc-level replace)
curl -s localhost:8642/v1/index -H "x-api-key: dev" -d '{"collection":"kb","doc_id":"d.pdf","chunks":[...]}'
```

Auth: `X-API-Key: <k>` or `Authorization: Bearer <k>`.
Deep pagination: pass the previous page's last hit's `cursor` as the next request's `"search_after"`.
Contract: `GET /openapi.json` (OpenAPI 3.0); observability: `GET /metrics` (Prometheus).

### 3.3 Python SDK + LangChain / LlamaIndex

```python
from fastsearch_client import FastsearchClient
from fastsearch_client.integrations import FastsearchRetriever, hits_to_llama_nodes

c = FastsearchClient("http://127.0.0.1:8642", api_key="dev")
c.index("kb", "report.pdf", chunks)          # chunks: list of docparse chunk dicts
hits = c.search("kb", "gross margin", mode="hybrid", top_k=8)

# LangChain: duck-compatible with get_relevant_documents/invoke, drops straight into an LCEL pipeline
retriever = FastsearchRetriever(c, "kb", mode="hybrid", top_k=8, highlight=True)
docs = retriever.invoke("why did gross margin decline")    # -> list[Document] (metadata includes citation_id)

# LlamaIndex: hits -> NodeWithScore
nodes = hits_to_llama_nodes(c.search("kb", "gross margin", top_k=8, highlight=True))
```

Dependencies are optional: when langchain/llama-index aren't installed, it falls back to local equivalent objects (same-shape `page_content`/`metadata`).

### 3.4 Library (Rust) / CLI

- **Library**: `fastsearch_engine::Engine` (`create_in_ram` / `open`) + `engine.search(req, acl)` / `resolve_citation`.
- **CLI**: `index` (docparse chunks) · `index-dir` (feed a folder) · `search` · `ingest` (PDF, `--features parse`) · `eval` (relevance gate).

---

## 4. A typical RAG / Agentic flow (recipe)

```
① Search      hybrid + highlight + filter   → top-K hits with citations (citation_id/snippet)
② Build prompt  splice snippet + citation_id into context, ask the LLM to cite [citation_id]
③ Generate    LLM produces an answer with [citation_id]
④ Trace       call resolve_citation on each citation_id in the answer → page+bbox → frontend deep link/highlight
```

**Agentic correction loop** (agent self-checks → re-searches):

- Insufficient recall → switch to `mode=hybrid`, loosen `filter`, raise `top_k`/`candidates`.
- Too broad → tighten `filter` (`eq/in` on kind/modality/page), `collapse` to cap N per doc to avoid flooding.
- "Find more like this" → `/v1/similar` (more_like_this, by a hit's `citation_id`).
- Paging → `search_after` (take the previous page's last `cursor`); consistent with fusion/rerank ordering, no overlap.
- Self-eval → `fastsearch eval --golden ... --baseline ...` (nDCG/recall/MRR regression gate).

---

## 5. Multi-tenant Agents (ACL cannot be bypassed)

Map an API key to a tenant and tags, **injected server-side** into every search/resolve — neither the LLM nor the client can exceed its permissions.

```bash
# key=tenant:tag1,tag2 ; semicolon-separated for multiple; empty tenant = no tenant restriction (admin)
FASTSEARCH_KEYS="alice=acme:team-a,public; bob=acme:team-b; admin=:public"
```

- `alice` (acme/team-a) sees only chunks whose `acl` contains `team-a` or `public` **and** `tenant=acme`.
- Same engine, same MCP/REST, different key → different visible set; **passing `acl`/`tenant` in the request body has no effect** (it's ignored).
- The media gateway `/v1/asset` enforces the same: unauthorized/nonexistent both return 404, leaking no existence information.

> Ideal for "one Agent service, many tenants": one key per tenant, retrieval is isolated automatically, no permission logic in the prompt/app layer.

---

## 6. Multimodal

> **Document parsing & OCR/tables**: `fastsearch ingest` parses 9 formats + images in-process; scanned docs get **PP-OCR** text and tables get **non-VLM ONNX structure recognition** — see **[Ingestion & parsing](文件解析与摄取.md)**.

Image captions and audio/video transcripts are ingested as searchable text; `modality` is a filterable field:

```json
{"query": "revenue trend chart", "filter": {"eq": ["modality", "image"]}}
```

Hits carry `media` (a media reference) and `time` (an audio/video interval); `resolve_citation` resolves the media location/signed URL (ACL enforced).

---

## 7. Production deployment and configuration (env cheat-sheet)

| Variable | Effect |
|---|---|
| `FASTSEARCH_DATA` | Index data directory (default `./data`) |
| `FASTSEARCH_KEYS` | API key table `key=tenant:tags;...` (unset = a single dev key, no tenant restriction) |
| `FASTSEARCH_EMBEDDER` | `ollama`\|`openai` (+ `FASTSEARCH_EMBED_*`) → real semantic embeddings (enables vector/hybrid) |
| `FASTSEARCH_VECTOR_BACKEND` | `brute` (deterministic default) \| `hnsw` (large-scale approximate) \| `pgvector` (direct query, needs `DATABASE_URL`) |
| `FASTSEARCH_CDC=1` | Enable background CDC: PG write → logical replication → auto-embed → index (needs `DATABASE_URL`) |
| `FASTSEARCH_RATE_LIMIT` | `cap,refill` (per-key token bucket; 429 on overflow) |
| `FASTSEARCH_AUDIT=1` | Emit an audit JSON line to stderr per successful request |
| `FASTSEARCH_TOKENIZER` | `jieba` (default, Chinese) \| `default` (whitespace split) |

- **Local semantic search**: `FASTSEARCH_EMBEDDER=ollama` (+ local ollama) → real semantic hybrid.
- **Container / K8s**: see [`deploy/`](../deploy/) (Dockerfile + docker-compose + CloudNativePG sample).
- **Capacity / SLO / HA**: see [Capacity & SLO](governance/2026-06-26-容量与SLO.md) (stateless multi-replica + derived & rebuildable).

---

## 8. Comparison with alternatives (an honest positioning)

| Dimension | fastsearch | Meilisearch | Qdrant / Weaviate | pgvector (bare) | Elasticsearch |
|---|---|---|---|---|---|
| Retrieval | keyword+vector **hybrid** (RRF) | keyword+vector hybrid | vector-first (+ some BM25) | vector | keyword+vector |
| **Traceable citation → deep link** | ✅ page+bbox + `resolve_citation` | highlight/attributes | metadata | DIY | highlight |
| **ACL cannot be bypassed** | ✅ server-side injection, multi-tenant | app layer / multi-tenancy | app layer | DIY | document-level security (commercial) |
| **MCP-native tools** | ✅ search/resolve_citation | ✅ (recently) | via third party | — | via third party |
| Source of truth / lock-in | **managed PG source of truth, rebuildable** | proprietary storage | proprietary storage | it *is* PG | proprietary storage |
| Filter-aware recall | ✅ superset + exact post-filter | ✅ | implementation-dependent | **post-filter easily loses recall** | ✅ |
| Deployment | single binary + any managed PG | single binary | cluster | inside PG | cluster |

**When to pick fastsearch**: you want to give an Agent a retrieval layer with **traceable citations + multi-tenant isolation**, and you want your data to stay in **your own managed Postgres** rather than locked into proprietary storage. For pure-semantic / massive-vector workloads, a dedicated vector store may fit better.

---

## References

- Meilisearch — [Build a RAG pipeline](https://www.meilisearch.com/blog/rag-with-meilisearch), [Agentic RAG](https://www.meilisearch.com/blog/agentic-rag), [Hybrid search RAG](https://www.meilisearch.com/blog/hybrid-search-rag), [RAG Infrastructure](https://www.meilisearch.com/products/rag)
- [Best AI Search Engines for Agents (Firecrawl, 2026)](https://www.firecrawl.dev/blog/best-ai-search-engines-agents)
- This project: [README](../README.md) · [Architecture/CLAUDE.md](../CLAUDE.md) · [Module specs](specs/00-模块拆分.md) · [REST OpenAPI](../crates/fastsearch-server/src/lib.rs) (`GET /openapi.json`)
