# fastsearch-client (TypeScript)

零依赖（全局 `fetch`）的 fastsearch REST SDK，**为 LLM agent 开发量身定制**。
Node 18+ / Deno / Bun / 浏览器通用。ACL 由服务端按 API Key 强制，客户端无法越权。

```bash
npm install fastsearch-client
```

## 快速上手

```ts
import { FastsearchClient, f } from "fastsearch-client";

const client = new FastsearchClient({
  baseUrl: "http://127.0.0.1:8642",
  apiKey: "dev",
});

// 灌入 docparse chunks（doc 级替换）
await client.index("kb", "report.pdf", chunks);

// 混合检索（带 page+bbox 引用溯源）
const { hits } = await client.search("kb", "毛利率为什么下降", {
  topK: 8,
  highlight: true,                       // 让正文片段进入 hit.highlight
  filter: f.and(f.eq("kind", "paragraph"), f.gte("page", 3)),
});
for (const h of hits) {
  console.log(h.citation_id, "p." + h.page, h.highlight);
}
```

## 给 Agent 加一个检索工具（核心）

`makeSearchTool` 一次产出 **Anthropic / OpenAI 两家的工具定义** + 一个可执行的 `run()`，
收到模型的 tool call 时调它，回灌 `content`（带 `[n]` 引用标记），并拿到 `citations` 供深链溯源。

### Anthropic（Claude）

```ts
import Anthropic from "@anthropic-ai/sdk";
import { FastsearchClient, makeSearchTool } from "fastsearch-client";

const client = new FastsearchClient({ baseUrl, apiKey: "dev" });
const tool = makeSearchTool(client, "kb");
const anthropic = new Anthropic();

const messages = [{ role: "user" as const, content: "毛利率为什么下降？" }];
let resp = await anthropic.messages.create({
  model: "claude-opus-4-8",
  max_tokens: 1024,
  tools: [tool.anthropic],            // ← 工具定义
  messages,
});

// 模型请求检索 → 执行 → 回灌 tool_result
while (resp.stop_reason === "tool_use") {
  messages.push({ role: "assistant", content: resp.content });
  const results = [];
  for (const block of resp.content) {
    if (block.type === "tool_use" && block.name === tool.name) {
      const out = await tool.run(block.input as any);   // ← 执行检索
      results.push({
        type: "tool_result" as const,
        tool_use_id: block.id,
        content: out.content,            // 带 [n] 标记的可引用上下文
      });
      // out.citations / out.hits 留作溯源、深链、UI 高亮
    }
  }
  messages.push({ role: "user", content: results });
  resp = await anthropic.messages.create({
    model: "claude-opus-4-8", max_tokens: 1024, tools: [tool.anthropic], messages,
  });
}
```

### OpenAI

```ts
const tool = makeSearchTool(client, "kb");
const completion = await openai.chat.completions.create({
  model: "gpt-4o",
  tools: [tool.openai],                 // ← OpenAI function 形态
  messages: [{ role: "user", content: "毛利率为什么下降？" }],
});
// 收到 tool_calls 时：const out = await tool.run(JSON.parse(call.function.arguments));
```

`makeSearchTool(client, collection, opts)` 可固定模型不可覆盖的服务端策略：

```ts
const tool = makeSearchTool(client, "kb", {
  defaultTopK: 6,
  fixed: { filter: f.eq("tenant_visible", true), autoMerge: true },
  format: { maxCharsPerHit: 400 },
});
```

## RAG 上下文拼装

不走工具调用、想自己拼 prompt 时：

```ts
import { formatHitsForLLM } from "fastsearch-client";

const { hits } = await client.search("kb", query, { topK: 8, highlight: true });
const { content, citations } = formatHitsForLLM(hits);
// content:  "[1] (report.pdf p.7) …片段…\n[2] (report.pdf p.9) …"
// citations: [{ marker: 1, citation_id, doc_id, page, ... }, ...]
const prompt = `根据以下资料回答，并用 [n] 标注来源：\n\n${content}\n\n问题：${query}`;
```

## LangChain.js 检索器

```ts
import { FastsearchRetriever } from "fastsearch-client";

const retriever = new FastsearchRetriever(client, "kb", { topK: 8 });
const docs = await retriever.invoke("毛利率");   // -> { pageContent, metadata }[]
// 鸭子兼容 LangChain Retriever，可进 LCEL：retriever.pipe(prompt).pipe(model)
```

## 更多能力

```ts
// 深分页（agent 全量扫读）
for await (const page of client.paginate("kb", query, { topK: 20, maxPages: 5 })) {
  // ...每页 hits
}

// more_like_this：按命中反查相似
const similar = await client.similar("kb:report.pdf:3", { topK: 5 });

// 把引用解析成可直接渲染的短时 URL / 跳原文（前端 <img src>）
const assets = await client.resolveAssets(hits.map((h) => h.citation_id));

// 健康探针
if (!(await client.health())) throw new Error("server down");
```

## 错误处理

`FastsearchError` 带 `status` 与分流判定，便于 agent 重试/重认证：

```ts
import { FastsearchError } from "fastsearch-client";
try {
  await client.search("kb", q);
} catch (e) {
  if (e instanceof FastsearchError) {
    if (e.isAuth) /* 401：换 key */;
    else if (e.isRetryable) /* 429/5xx：退避重试 */;
  }
}
```

构造时也可开内建重试：`new FastsearchClient({ baseUrl, apiKey, retries: 2, timeoutMs: 15000 })`。

## 取舍说明（诚实）

`/v1/search` 出于载荷精简**不回整段正文**，仅在 `highlight: true` 时回高亮片段。故
`formatHitsForLLM` / 检索器的正文取高亮片段；完整正文与 page/bbox 深链经 `citation_id`
走 `resolveAssets` / 答案层解析。ACL 由服务端按 API Key 强制，SDK 无法越权。

## 开发

```bash
npm run build       # 编译到 dist/
npm run typecheck   # 仅类型检查
npm test            # node --test（零网络，stub fetch）
```

许可 Apache-2.0。
