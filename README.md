# fastsearch

> 🌏 中文版: [README.zh-CN.md](README.zh-CN.md)

**A single-binary, external hybrid-search engine that treats managed Postgres (pgvector) as the source of truth.** It runs full-text / vector / hybrid retrieval over traceable document chunks (parsed by docparse-rs, or plain text / markdown) and carries **page+bbox citations end-to-end** to the answer layer — purpose-built for **retrieval and grounding in AI Agents / RAG**.

> 👉 **Building an Agent? Start with [Using fastsearch in an Agent](docs/using-fastsearch-in-an-agent.md)** (the four faces · RAG recipe · MCP · multi-tenant ACL · comparison with alternatives).

> The key edge over ParadeDB: **it runs on any managed Postgres (RDS / Supabase / Neon)** — it only needs pgvector + logical replication, and **requires no `shared_preload_libraries` native extension**.

## Architecture

```
docparse chunks / text files → Postgres (source of truth: chunk + metadata + ACL + pgvector)
                       │ logical-replication CDC (pgoutput, idempotent, LSN resumable)
                       ▼
   fastsearch engine (single binary, stateless multi-replica, derived index rebuildable)
     · BM25 inverted index (Tantivy/mmap)   · vector (brute-force / HNSW+u8 quant / pgvector direct, filter-aware)
     · fusion (RRF / normalized / weighted) · per-document ACL enforced server-side (cannot be bypassed)
     · citation traceability (page+bbox+section) + resolve_citation deep links · multimodal
   Four faces: CLI · library · REST · MCP
```

## Five-minute quickstart (zero deps, runs locally)

```bash
cargo build -p fastsearch-cli --bin fastsearch
# Feed it a folder (recurses .md/.txt; markdown headings become breadcrumbs), then search
./target/debug/fastsearch index-dir --data ./idx --collection kb  ./my-docs
./target/debug/fastsearch search    --data ./idx --collection kb --query "gross margin" --json
```

For docparse / PDF / REST / MCP / Python usage, see the [Agent usage guide](docs/在Agent中使用fastsearch.md).

## Modules (workspace crates)

| crate | Responsibility |
|---|---|
| `fastsearch-core` | Document model, query/filter AST, fusion (RRF / normalized / weighted), citations, **ACL** |
| `fastsearch-text` | Tantivy BM25 + CJK (jieba) + filtering + highlighting/facets + ACL |
| `fastsearch-vector` | Three vector backends: brute-force (deterministic default) / HNSW+u8 quant (approximate) / pgvector direct; filter-aware |
| `fastsearch-embed` | Embedder trait + configurable HTTP backend (Ollama / OpenAI-compatible) |
| `fastsearch-pg` | Postgres source of truth: DDL, Chunk↔row mapping, doc-level replace write path, pgvector direct query |
| `fastsearch-sync` | CDC apply: pgoutput decode + idempotency + LSN checkpoint + replace semantics |
| `fastsearch-engine` | Orchestration: ingest→CDC→index→**full-text / vector / hybrid** search→citations + deep pagination + rebuild + media resolution |
| `fastsearch-eval` | Relevance evaluation: golden set + nDCG/recall/MRR + CI regression gate |
| `fastsearch-server` | REST (axum) + API-key auth + **ACL cannot be bypassed** + metrics/rate-limit/audit + media gateway + CDC lifecycle |
| `fastsearch-mcp` | The fourth face: MCP (stdio + JSON-RPC) exposing the `search` / `resolve_citation` tools |
| `fastsearch-cli` | `fastsearch` binary: index / index-dir / search / **ingest (multi-format: PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML/image + OCR + table recognition)** / eval — see [Ingestion & parsing](docs/ingestion-and-parsing.md) |
| `clients/{python,ts}` | Zero-dependency SDKs + LangChain / LlamaIndex adapters |

**End-to-end usable**: ingest/CDC → index → three search modes (keyword / vector / hybrid) → hits with citations, ACL enforced and unbypassable. All four faces in place.

## Build & test

```bash
cargo test --workspace                                    # all green (PG integration runs when DATABASE_URL is set)
cargo clippy --workspace --all-targets -- -D warnings     # zero warnings
cargo fmt --all --check
DATABASE_URL=postgres://... cargo test -p fastsearch-pg   # PG integration (CI uses the pgvector/pgvector image + wal_level=logical)
```

## Documentation

- **[Using fastsearch in an Agent](docs/using-fastsearch-in-an-agent.md)** (developer usage guide)
- **[Ingestion & parsing](docs/ingestion-and-parsing.md)** — multi-format ingest, OCR, table recognition, build tiers, model setup
- [Architecture cheat-sheet / commands / invariants (CLAUDE.md)](CLAUDE.md)
- [Module breakdown & spec index](docs/specs/00-模块拆分.md)
- [Requirements analysis](docs/plans/2026-06-24-需求分析报告.md) · [Product design](docs/plans/2026-06-24-产品设计文档.md)
- [Deployment](deploy/) · [Capacity & SLO](docs/governance/2026-06-26-容量与SLO.md)

## License

Apache-2.0. Tokenization dictionaries come from jieba-rs (**MIT**, with embedded dict) — no share-alike obligation (e.g. CC-BY-SA); shipping the MIT attribution is sufficient (see the [license review](docs/governance/2026-06-26-词典与第三方许可审.md)).
