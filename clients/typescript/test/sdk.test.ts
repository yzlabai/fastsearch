// SDK 单测（零网络：注入 stub fetch；纯助手直接断言）。
// 跑：npm test（编译到 .testbuild 后 node --test）。

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  FastsearchClient,
  FastsearchError,
  f,
  formatHitsForLLM,
  hitToDocument,
  makeSearchTool,
  type Hit,
} from "../src/index.js";

// ---- 录制型 stub fetch ----------------------------------------------------

interface Recorded {
  url: string;
  method: string;
  body: unknown;
  headers: Record<string, string>;
}

function stub(
  responder: (rec: Recorded) => {
    status?: number;
    json?: unknown;
    text?: string;
    body?: BodyInit;
    contentType?: string;
  },
): { fetch: typeof fetch; calls: Recorded[] } {
  const calls: Recorded[] = [];
  const fetchImpl = (async (url: string | URL, init?: RequestInit) => {
    const headers: Record<string, string> = {};
    for (const [k, v] of Object.entries(init?.headers ?? {})) {
      headers[k.toLowerCase()] = String(v);
    }
    const rec: Recorded = {
      url: String(url),
      method: init?.method ?? "GET",
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
      headers,
    };
    calls.push(rec);
    const r = responder(rec);
    const status = r.status ?? 200;
    const payload = r.body ?? r.text ?? JSON.stringify(r.json ?? {});
    return new Response(payload, {
      status,
      headers: { "content-type": r.contentType ?? "application/json" },
    });
  }) as unknown as typeof fetch;
  return { fetch: fetchImpl, calls };
}

function makeHit(over: Partial<Hit> = {}): Hit {
  return {
    citation_id: "kb:report.pdf:3",
    score: 0.9,
    bm25: 0.5,
    vector: 0.4,
    rerank: null,
    doc_id: "report.pdf",
    chunk_id: 3,
    page: 7,
    bbox: { x0: 0, y0: 0, x1: 1, y1: 1 },
    heading_path: ["财务", "毛利率"],
    section_id: 2,
    highlight: "毛利率下降的原因是…",
    merged_chunk_ids: [],
    cursor: "cur-3",
    ...over,
  };
}

// ---- filter 构造器 --------------------------------------------------------

test("filter builder produces correct AST", () => {
  const filter = f.and(
    f.eq("kind", "table"),
    f.gte("page", 10),
    f.in("doc_id", ["a.pdf", "b.pdf"]),
    f.not(f.exists("draft")),
    f.headingPrefix("财务", "毛利率"),
  );
  assert.deepEqual(filter, {
    and: [
      { eq: ["kind", "table"] },
      { gte: ["page", 10] },
      { in: ["doc_id", ["a.pdf", "b.pdf"]] },
      { not: { exists: "draft" } },
      { heading_prefix: ["财务", "毛利率"] },
    ],
  });
});

// ---- search 入参映射 ------------------------------------------------------

test("search maps camelCase options to snake_case body", async () => {
  const { fetch, calls } = stub(() => ({ json: { hits: [makeHit()], facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const res = await c.search("kb", "毛利率", {
    topK: 5,
    autoMerge: true,
    searchAfter: "cur-1",
    highlight: true,
    filter: f.eq("kind", "table"),
    facets: ["doc_id"],
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0]!.url, "http://x/v1/search");
  assert.equal(calls[0]!.headers["x-api-key"], "dev");
  assert.deepEqual(calls[0]!.body, {
    query: "毛利率",
    mode: "hybrid",
    top_k: 5,
    auto_merge: true,
    search_after: "cur-1",
    highlight: true,
    filter: { eq: ["kind", "table"] },
    facets: ["doc_id"],
  });
  assert.equal(res.hits.length, 1);
});

test("search maps queryImage and embedder", async () => {
  const { fetch, calls } = stub(() => ({ json: { hits: [], facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  await c.search("kb", "找相似图", {
    mode: "vector",
    queryImage: [255, 0, 16],
    embedder: "clip",
  });
  const body = calls[0]!.body as Record<string, unknown>;
  assert.deepEqual(body.query_image, [255, 0, 16]);
  assert.equal(body.embedder, "clip");
});

test("searchHits returns just the array", async () => {
  const { fetch } = stub(() => ({ json: { hits: [makeHit(), makeHit()], facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const hits = await c.searchHits("kb", "q");
  assert.equal(hits.length, 2);
});

// ---- 错误分流 -------------------------------------------------------------

test("401 surfaces as FastsearchError with isAuth", async () => {
  const { fetch } = stub(() => ({ status: 401, text: "missing or invalid API key" }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "bad", fetch });
  await assert.rejects(
    () => c.search("kb", "q"),
    (e: unknown) => {
      if (!(e instanceof FastsearchError)) throw new Error("expected FastsearchError");
      assert.equal(e.status, 401);
      assert.ok(e.isAuth);
      assert.ok(!e.isRetryable);
      return true;
    },
  );
});

test("retries on 503 then succeeds", async () => {
  let n = 0;
  const { fetch, calls } = stub(() => {
    n += 1;
    return n < 2 ? { status: 503, text: "busy" } : { json: { hits: [], facets: {} } };
  });
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch, retries: 2 });
  const res = await c.search("kb", "q");
  assert.equal(calls.length, 2);
  assert.equal(res.hits.length, 0);
});

// ---- index 映射 -----------------------------------------------------------

test("index maps id->chunk_id and applies default acl", async () => {
  const { fetch, calls } = stub(() => ({ json: { indexed: 1 } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const n = await c.index("kb", "r.pdf", [{ id: 9, text: "hi" }]);
  assert.equal(n, 1);
  const body = calls[0]!.body as { chunks: Record<string, unknown>[] };
  assert.equal(body.chunks[0]!.chunk_id, 9);
  assert.equal(body.chunks[0]!.id, undefined);
  assert.equal(body.chunks[0]!.doc_id, "r.pdf");
  assert.deepEqual(body.chunks[0]!.acl, ["public"]);
});

// ---- similar --------------------------------------------------------------

test("similar posts citation_id and top_k", async () => {
  const { fetch, calls } = stub(() => ({ json: { hits: [makeHit()] } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const hits = await c.similar("kb:r.pdf:3", { topK: 4 });
  assert.deepEqual(calls[0]!.body, { citation_id: "kb:r.pdf:3", top_k: 4 });
  assert.equal(hits.length, 1);
});

// ---- paginate / 资产 ------------------------------------------------------

test("paginate stops when the last hit lacks a cursor (no infinite loop)", async () => {
  // 满页但末条无 cursor → 生成器应停，而非从第一页重头取。
  const full = [
    makeHit({ cursor: "" }),
    { ...makeHit(), cursor: undefined } as unknown as Hit,
  ];
  const { fetch, calls } = stub(() => ({ json: { hits: full, facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const pages: number[] = [];
  for await (const page of c.paginate("kb", "q", { topK: 2 })) {
    pages.push(page.length);
    if (pages.length > 3) break; // 安全阀：若死循环则强制退出并让断言失败
  }
  assert.deepEqual(pages, [2]);
  assert.equal(calls.length, 1);
});

test("fetchAssetBytes returns bytes for binary, null for doc_render JSON", async () => {
  const { fetch } = stub((rec) =>
    rec.url.includes("img")
      ? { body: new Uint8Array([1, 2, 3]), contentType: "image/png" }
      : { json: { type: "doc_render", page: 7 }, contentType: "application/json" },
  );
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const bin = await c.fetchAssetBytes("kb:img.pdf:1");
  assert.ok(bin);
  assert.equal(bin!.contentType, "image/png");
  assert.deepEqual([...bin!.bytes], [1, 2, 3]);
  const docRender = await c.fetchAssetBytes("kb:text.pdf:1");
  assert.equal(docRender, null);
});

test("fetchAssetBytes returns null on 404", async () => {
  const { fetch } = stub(() => ({ status: 404, text: "not found" }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  assert.equal(await c.fetchAssetBytes("kb:x:1"), null);
});

// ---- agent 助手 -----------------------------------------------------------

test("formatHitsForLLM builds markers and parallel citations", () => {
  const ctx = formatHitsForLLM([makeHit(), makeHit({ citation_id: "kb:r.pdf:4", page: 8 })]);
  assert.match(ctx.content, /^\[1\] \(report\.pdf p\.7\) 毛利率下降/);
  assert.match(ctx.content, /\[2\] \(report\.pdf p\.8\)/);
  assert.equal(ctx.citations.length, 2);
  assert.equal(ctx.citations[0]!.marker, 1);
  assert.equal(ctx.citations[1]!.citation_id, "kb:r.pdf:4");
});

test("makeSearchTool exposes both tool schemas and runs", async () => {
  const { fetch, calls } = stub(() => ({ json: { hits: [makeHit()], facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const tool = makeSearchTool(c, "kb", { defaultTopK: 3 });
  assert.equal(tool.anthropic.name, "search_knowledge_base");
  assert.equal(tool.openai.type, "function");
  assert.deepEqual(tool.anthropic.input_schema.required, ["query"]);

  const result = await tool.run({ query: "毛利率" });
  // 默认 highlight=true、topK 取 defaultTopK。
  assert.deepEqual(calls[0]!.body, {
    query: "毛利率",
    mode: "hybrid",
    top_k: 3,
    highlight: true,
  });
  assert.equal(result.hits.length, 1);
  assert.match(result.content, /\[1\]/);
  assert.equal(result.citations[0]!.citation_id, "kb:report.pdf:3");
});

test("makeSearchTool returns friendly message on empty query and no hits", async () => {
  const { fetch } = stub(() => ({ json: { hits: [], facets: {} } }));
  const c = new FastsearchClient({ baseUrl: "http://x", apiKey: "dev", fetch });
  const tool = makeSearchTool(c, "kb");
  assert.match((await tool.run({ query: "" })).content, /空查询/);
  assert.match((await tool.run({ query: "无关" })).content, /未检索到/);
});

test("hitToDocument maps highlight to pageContent and rest to metadata", () => {
  const doc = hitToDocument(makeHit());
  assert.equal(doc.pageContent, "毛利率下降的原因是…");
  assert.equal(doc.metadata.citation_id, "kb:report.pdf:3");
  assert.equal(doc.metadata.page, 7);
  assert.equal("highlight" in doc.metadata, false);
});
