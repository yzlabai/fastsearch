# Knowledge-Base Agent Example (Hono · Drizzle/SQLite · AI SDK · Vite/React/shadcn)

English | [简体中文](./README.zh-CN.md)

An end-to-end "knowledge-base agent" mini-app whose retrieval backend is this repo's **fastsearch** REST engine.

```
Browser (Vite/React/shadcn chat UI)
   │  /api  →  Hono server (:8787)
   │            ├─ /api/chat       Vercel AI SDK · streamText + tool loop (DeepSeek)
   │            │      └─ tool searchKnowledgeBase ──→ fastsearch /v1/search
   │            └─ /api/documents  chunk ──→ fastsearch /v1/index  + register in SQLite (Drizzle)
   ▼
fastsearch-server (:8642)  ← hybrid search engine (Rust crate in this repo)
```

- **Hono**: backend HTTP (streaming chat endpoint + document ingest endpoint).
- **SQLite + Drizzle**: local store for the "document list" and "chat history" (the source of truth for searchable content lives in fastsearch/PG; this is just a registry).
- **Vercel AI SDK**: the agent itself — `streamText` runs a tool loop, default model **DeepSeek (`deepseek-v4-flash`)**; the `searchKnowledgeBase` tool calls fastsearch for evidence.
- **`fastsearch-client`** (the published npm SDK): the retrieval client. `makeSearchTool` turns a collection into a ready agent tool (`run()` does search + `[n]`-marked context + citations); no hand-rolled REST client.
- **Vite + React + shadcn/ui**: chat interface + document ingest panel, answers carry traceable `citation_id`s.

> This is an *example* shipped alongside fastsearch — it shows "how to build a RAG agent on top of fastsearch."

## Prerequisites

- Node ≥ 20.12 (uses `process.loadEnvFile`)
- A DeepSeek API key ([platform.deepseek.com](https://platform.deepseek.com))
- Be able to `cargo run` from the repo root (to start fastsearch-server)

## Run it (3 terminals)

**① Start the fastsearch engine** (at the repo root, not `example/`)

```bash
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" \
  cargo run -p fastsearch-server --bin fastsearch-server
# Listens on :8642, API Key = dev. No embedding backend → pure keyword (BM25) mode, enough for this example.
```

**② Install deps + configure .env** (in `example/`)

```bash
cd example
npm install
cp .env.example .env
# Edit .env — at minimum set DEEPSEEK_API_KEY
```

**③ Start frontend + backend**

```bash
npm run dev
# Hono backend  http://127.0.0.1:8787
# Frontend      http://127.0.0.1:5173   ← open this
```

Open 5173: paste a document on the left and "ingest" it, then ask questions on the right. The agent searches first, then answers with `[kb:doc:chunk]` citations.

## Tests

**Unit / integration** — fast, no external services (an in-process fake fastsearch server stands in for the engine):

```bash
npm test
```

Covers the naive chunker (`chunkText`) and the SDK wrapper + agent tool wiring (the write path `indexDoc` request shape; the `searchKnowledgeBase` tool's `search(highlight:true)` call and its `content`/`citations`/`hits` output). Uses Node's built-in test runner via tsx.

**End-to-end smoke** — drives the live stack. With it up (① + ③ both running, `.env` has `DEEPSEEK_API_KEY`), open another terminal:

```bash
npm run test:e2e
```

It hits the live server and validates the whole chain: health check → ingest a doc (`/v1/index` + SQLite) → `/api/chat` agent tool loop (calls `searchKnowledgeBase` → gets fastsearch hits → answers with citations → finishes cleanly). Zero dependencies (just Node's built-in `fetch`); exits 0 when all green. Script: `test/smoke.mjs`.

## Want semantic search (vectors)?

Just configure an embedding backend for fastsearch (Ollama / OpenAI-compatible) — no change needed in this example. The `/api/chat` tool already uses `mode: "hybrid"`, so once the engine has embeddings it recalls vectors automatically. See the repo-root `CLAUDE.md` and `crates/fastsearch-embed`.

## Code tour

```
src/server/
  index.ts              Hono entry (serve :8787)
  env.ts                loads .env first
  db/schema.ts          Drizzle tables: documents / chunks / messages
  db/index.ts           better-sqlite3 + boot-time CREATE TABLE IF NOT EXISTS
  lib/fastsearch.ts     fastsearch-client SDK singleton (+ local chunk types for the naive chunker)
  lib/chunk.ts          naive chunker (real pipelines use docparse)
  lib/agent.ts          model + system prompt + makeSearchTool() from fastsearch-client
  routes/chat.ts        AI SDK streamText tool loop, persists each turn
  routes/documents.ts   ingest: chunk → /v1/index → register in SQLite; list
src/web/
  App.tsx               two-pane layout
  components/Chat.tsx          useChat chat + renders tool sources
  components/DocumentsPanel.tsx document ingest + list
  components/ui/*       shadcn primitives (button/card/input/textarea/badge)
test/
  chunk.test.ts         unit: naive chunker (npm test)
  wrapper.test.ts       integration: SDK wrapper + agent tool vs a fake fastsearch (npm test)
  smoke.mjs             end-to-end smoke test (npm run test:e2e)
```

## Invariants (inherited from fastsearch's constraints)

- **ACL is not bypassable**: read/write permissions are enforced server-side by fastsearch per API key — clients can't pass or loosen ACL.
- **PG is the source of truth, the engine index is derived**: the SQLite here only holds a local registry/cache, never authoritative data.
- **Honest accounting**: with no embedding backend it's pure keyword mode — don't mistake it for semantic search.
