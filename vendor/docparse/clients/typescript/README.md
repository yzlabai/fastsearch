# docparse-client (TypeScript)

Thin TypeScript/Node client for [docparse](https://github.com/yzlabai/docparse-rs) —
the pure-Rust document parser. **Zero runtime dependencies**: wraps the
`docparse` binary (subprocess) or a running `docparse serve` (REST via global
`fetch`). Node ≥ 18.

```bash
npm install docparse-client     # + put the `docparse` binary on PATH (or set DOCPARSE_BIN)
```

## Parse

```ts
import { DocparseClient } from 'docparse-client';

const client = new DocparseClient();                  // finds docparse on PATH / $DOCPARSE_BIN
const doc    = await client.parse('paper.pdf');       // full IR (provenance + coordinates)
const chunks = await client.chunks('paper.pdf');      // RAG chunks: text + page + bbox + breadcrumb
const md     = await client.parse('paper.pdf', { format: 'markdown' });
const toc    = await client.outline('paper.pdf');     // structure tree (nested sections)
```

Against a long-running server (`docparse serve --port 8642`):

```ts
import { DocparseHttpClient } from 'docparse-client';
const http   = new DocparseHttpClient({ baseUrl: 'http://127.0.0.1:8642' });
const chunks = await http.chunks('paper.pdf');
const schema = await http.schema('chunk');            // the JSON Schema, for validation/codegen
```

Enhancements are opt-in (`{ ocr, layout, tableModel, formulaModel }`); digital
documents never touch a model. The server must be started with the matching
model config for the HTTP transport.

## LangChain.js

```ts
import { DocparseLoader } from 'docparse-client/langchain';

const docs = await new DocparseLoader('paper.pdf').load();
docs[0].metadata;   // { source, page, bbox, heading_path, section_id, kind }
```

Every `Document` carries `page` + `bbox` — answers cite back to a highlightable
region of the source. Needs `@langchain/core` (optional peer).

## Vercel AI SDK

```ts
import { generateText } from 'ai';
import { docparseTools } from 'docparse-client/ai';

const result = await generateText({
  model,
  tools: await docparseTools(),     // get_chunks / outline / parse_markdown
  prompt: 'Summarize report.pdf with page citations.',
});
```

Needs `ai` + `zod` (optional peers; `parameters`-style, AI SDK v3/v4).

## Types

The exported types (`Chunk`, `Section`, `BBox`, …) mirror the JSON contract. The
authoritative schemas live in the repo (`schemas/*.json`, draft 2020-12) and are
served at `GET /openapi.json` / `GET /schema/{name}` — run `quicktype` over them
for exhaustive generated types.

## Develop

```bash
npm install && npm run build          # → dist/
DOCPARSE_BIN=../../target/release/docparse npm test
```
