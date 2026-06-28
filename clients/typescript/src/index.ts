// fastsearch TypeScript SDK（零依赖，使用全局 fetch；Node 18+ / Deno / Bun / 浏览器）。
//
// 封装 fastsearch-server 的 REST API，并提供面向 agent 开发的助手（工具定义、RAG 上下文拼装、
// LangChain.js 风格检索器）。ACL 由服务端按 API Key 强制，客户端无法越权。
//
// 快速上手：
//   import { FastsearchClient, makeSearchTool, f } from "fastsearch-client";
//   const client = new FastsearchClient({ baseUrl: "http://127.0.0.1:8642", apiKey: "dev" });
//   await client.index("kb", "report.pdf", chunks);              // 灌入 docparse chunks
//   const { hits } = await client.search("kb", "毛利率", {        // 混合检索
//     topK: 8, highlight: true, filter: f.eq("kind", "table"),
//   });
//   const tool = makeSearchTool(client, "kb");                   // 挂到 Claude/OpenAI 的工具

export { FastsearchClient } from "./client.js";
export type { ClientConfig, IndexOptions } from "./client.js";
export { FastsearchError } from "./errors.js";
export { f } from "./filter.js";
export {
  formatHitsForLLM,
  hitToDocument,
  makeSearchTool,
  FastsearchRetriever,
} from "./agent.js";
export type {
  AnthropicToolDef,
  Citation,
  FormatOptions,
  LlmContext,
  MakeSearchToolOptions,
  OpenAIToolDef,
  RetrievedDocument,
  SearchToolInput,
  SearchToolResult,
} from "./agent.js";
export type {
  BBox,
  Collapse,
  FacetValue,
  FieldValue,
  Filter,
  Fusion,
  Hit,
  Media,
  RerankSpec,
  ResolvedAsset,
  SearchMode,
  SearchOptions,
  SearchResponse,
} from "./types.js";
