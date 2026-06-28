// 我们对 SDK 的封装 + 工具接线的集成测试：用一个**进程内假 fastsearch 服务**
// 顶替真引擎（免起 cargo / 不连 DeepSeek），验证：
//   1) indexDoc 的写路径——doc 级替换的请求体形状、返回写入条数；
//   2) searchKnowledgeBase 工具——search(highlight=true) 的请求 + 拼装的 content/citations/hits。
// 跑法：npm test（node --import tsx --test）。
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";

// 假服务收到的请求体，按端点归档供断言。
const received: { index: any[]; search: any[] } = { index: [], search: [] };

const CANNED_HITS = [
  {
    citation_id: "kb:report.pdf:0",
    score: 1.2345,
    bm25: 1.2345,
    vector: null,
    doc_id: "report.pdf",
    chunk_id: 0,
    page: 7,
    bbox: { x0: 0, y0: 0, x1: 0, y1: 0 },
    heading_path: ["财务", "毛利"],
    section_id: 0,
    highlight: "毛利率因原材料成本上升而下降。",
  },
];

function readJson(req: http.IncomingMessage): Promise<any> {
  return new Promise((resolve, reject) => {
    let buf = "";
    req.on("data", (c) => (buf += c));
    req.on("end", () => {
      try {
        resolve(buf ? JSON.parse(buf) : {});
      } catch (e) {
        reject(e);
      }
    });
  });
}

let server: http.Server;

before(async () => {
  server = http.createServer(async (req, res) => {
    const url = req.url ?? "";
    const json = (code: number, body: unknown) => {
      res.writeHead(code, { "content-type": "application/json" });
      res.end(JSON.stringify(body));
    };
    if (req.method === "GET" && url === "/healthz") return json(200, { ok: true });
    if (req.method === "POST" && url === "/v1/index") {
      const body = await readJson(req);
      received.index.push(body);
      return json(200, { indexed: body.chunks?.length ?? 0 });
    }
    if (req.method === "POST" && url === "/v1/search") {
      const body = await readJson(req);
      received.search.push(body);
      return json(200, { hits: CANNED_HITS, facets: {} });
    }
    res.writeHead(404).end("not found");
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
  const port = (server.address() as AddressInfo).port;
  // 必须在 import 被测模块**之前**设好——客户端单例在模块求值时读这些 env。
  process.env.FASTSEARCH_URL = `http://127.0.0.1:${port}`;
  process.env.FASTSEARCH_API_KEY = "dev";
  process.env.FASTSEARCH_COLLECTION = "kb";
});

after(async () => {
  await new Promise<void>((r) => server.close(() => r()));
});

test("indexDoc：doc 级替换请求体 + 返回写入条数（写路径）", async () => {
  const { indexDoc, COLLECTION } = await import("../src/server/lib/fastsearch.ts");
  const { chunkText } = await import("../src/server/lib/chunk.ts");

  const chunks = chunkText("report.pdf", "第一段内容。\n\n第二段更长一点的内容描述。");
  const n = await indexDoc("report.pdf", chunks);

  assert.equal(n, chunks.length);
  const sent = received.index.at(-1);
  assert.equal(sent.collection, COLLECTION);
  assert.equal(sent.doc_id, "report.pdf");
  assert.equal(sent.chunks.length, chunks.length);
  // 我们的 chunk 自带 acl=["public"]，SDK 应原样保留（不被默认值覆盖）。
  assert.deepEqual(sent.chunks[0].acl, ["public"]);
  assert.equal(sent.chunks[0].doc_id, "report.pdf");
  assert.equal(typeof sent.chunks[0].chunk_id, "number");
});

test("searchKnowledgeBase 工具：search(highlight=true) + content/citations/hits", async () => {
  const { tools } = await import("../src/server/lib/agent.ts");

  const out: any = await tools.searchKnowledgeBase.execute!(
    { query: "毛利率为什么下降" },
    { toolCallId: "t1", messages: [] } as any,
  );

  // 1) 工具确实带高亮发起了混合检索。
  const req = received.search.at(-1);
  assert.equal(req.query, "毛利率为什么下降");
  assert.equal(req.mode, "hybrid");
  assert.equal(req.highlight, true);
  assert.equal(req.top_k, 8); // 未传 topK → 默认 8

  // 2) content 带 [n] 标记 + 出处；高亮片段进入上下文。
  assert.match(out.content, /\[1\]/);
  assert.match(out.content, /report\.pdf/);
  assert.match(out.content, /毛利率因原材料成本上升而下降/);

  // 3) citations：marker→citation_id 对齐，供模型在答案里标注溯源。
  assert.equal(out.citations.length, 1);
  assert.equal(out.citations[0].marker, 1);
  assert.equal(out.citations[0].citation_id, "kb:report.pdf:0");
  assert.equal(out.citations[0].page, 7);

  // 4) hits：前端 Chat.tsx 读 output.hits[].citation_id 渲染"来源"徽章。
  assert.equal(out.hits.length, 1);
  assert.equal(out.hits[0].citation_id, "kb:report.pdf:0");
  assert.equal(out.hits[0].page, 7);
  assert.equal(out.hits[0].score, 1.2345); // 四舍五入到 4 位
});

test("searchKnowledgeBase 工具：topK 透传给检索", async () => {
  const { tools } = await import("../src/server/lib/agent.ts");
  await tools.searchKnowledgeBase.execute!(
    { query: "现金流", topK: 3 },
    { toolCallId: "t2", messages: [] } as any,
  );
  assert.equal(received.search.at(-1).top_k, 3);
});
