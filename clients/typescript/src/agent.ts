// Agent 开发助手：把 fastsearch 检索接到 LLM 的工具调用（tool use / function calling）与
// RAG 上下文拼装。目标是让"给 agent 加一个知识库检索工具"是几行代码的事。
//
//   import { FastsearchClient, makeSearchTool } from "fastsearch-client";
//   const client = new FastsearchClient({ baseUrl, apiKey: "dev" });
//   const tool = makeSearchTool(client, "kb");
//   // 1) 把 tool.anthropic 放进 Anthropic Messages 的 `tools`
//   // 2) 收到 tool_use 时：const result = await tool.run(input);
//   //    把 result.content 作为 tool_result 回灌；result.citations 供溯源/深链。

import type { FastsearchClient } from "./client.js";
import type { Filter, Hit, SearchMode, SearchOptions } from "./types.js";

// ---- LLM 上下文拼装 -------------------------------------------------------

/** 一条可引用的来源（与上下文里的 `[n]` 标记一一对应）。 */
export interface Citation {
  /** 1-based 引用序号（出现在拼装文本里的 `[n]`）。 */
  marker: number;
  citation_id: string;
  doc_id: string;
  page: number;
  heading_path: string[];
  score: number;
}

/** 拼装结果：可直接喂 LLM 的文本 + 平行的来源表。 */
export interface LlmContext {
  /** 形如 `[1] (report.pdf p.7) …片段…` 的带标记上下文块。 */
  content: string;
  citations: Citation[];
}

export interface FormatOptions {
  /** 单条片段最大字符数（截断防超长），默认不截断。 */
  maxCharsPerHit?: number;
  /** 是否在每条前加 `(doc_id p.N)` 出处，默认 true。 */
  showSource?: boolean;
}

/** 取一条命中的正文：优先高亮片段，回退标题路径（search 不回整段正文）。 */
function hitText(h: Hit): string {
  if (h.highlight && h.highlight.trim()) return h.highlight.trim();
  if (h.heading_path.length) return h.heading_path.join(" › ");
  return "";
}

/**
 * 把命中拼成"带 `[n]` 引用标记"的 LLM 上下文块 + 平行来源表。
 *
 * 注：`/v1/search` 载荷精简、不回整段正文；`content` 取高亮片段（检索时传 `highlight: true`），
 * 完整正文/深链经 `citation_id` 走 resolveAssets / 答案层解析。
 */
export function formatHitsForLLM(
  hits: Hit[],
  opts: FormatOptions = {},
): LlmContext {
  const { maxCharsPerHit, showSource = true } = opts;
  const citations: Citation[] = [];
  const lines: string[] = [];
  hits.forEach((h, i) => {
    const marker = i + 1;
    let text = hitText(h);
    if (maxCharsPerHit && text.length > maxCharsPerHit) {
      text = text.slice(0, maxCharsPerHit) + "…";
    }
    const src = showSource ? ` (${h.doc_id} p.${h.page})` : "";
    lines.push(`[${marker}]${src} ${text}`.trimEnd());
    citations.push({
      marker,
      citation_id: h.citation_id,
      doc_id: h.doc_id,
      page: h.page,
      heading_path: h.heading_path,
      score: h.score,
    });
  });
  return { content: lines.join("\n"), citations };
}

// ---- 工具 schema（与执行体） ----------------------------------------------

/** JSON Schema 形态的检索工具入参（Anthropic / OpenAI 通用 `input_schema`/`parameters`）。 */
const SEARCH_INPUT_SCHEMA = {
  type: "object",
  properties: {
    query: {
      type: "string",
      description: "自然语言检索词（支持中英文混合）。",
    },
    mode: {
      type: "string",
      enum: ["keyword", "vector", "hybrid"],
      description: "检索模式：keyword=关键词，vector=语义，hybrid=混合（默认）。",
    },
    top_k: {
      type: "integer",
      description: "返回条数，默认 8。需要更全面时调大。",
    },
  },
  required: ["query"],
} as const;

/** Anthropic Messages API 的工具定义形态。 */
export interface AnthropicToolDef {
  name: string;
  description: string;
  input_schema: typeof SEARCH_INPUT_SCHEMA;
}

/** OpenAI Chat Completions / Responses 的工具定义形态。 */
export interface OpenAIToolDef {
  type: "function";
  function: {
    name: string;
    description: string;
    parameters: typeof SEARCH_INPUT_SCHEMA;
  };
}

/** LLM 把工具调到这里的入参（与 SEARCH_INPUT_SCHEMA 对应）。 */
export interface SearchToolInput {
  query: string;
  mode?: SearchMode;
  top_k?: number;
}

/** 工具执行结果：`content` 直接回灌为 tool_result，`citations`/`hits` 供宿主溯源。 */
export interface SearchToolResult extends LlmContext {
  hits: Hit[];
}

export interface MakeSearchToolOptions {
  /** 工具名（默认 `search_knowledge_base`）。 */
  name?: string;
  /** 工具描述（给 LLM 看的；默认给出通用 RAG 说明）。 */
  description?: string;
  /** 默认返回条数（LLM 未指定 top_k 时），默认 8。 */
  defaultTopK?: number;
  /** 固定的检索模式/过滤等（LLM 不可覆盖的服务端策略）。 */
  fixed?: Pick<SearchOptions, "filter" | "fusion" | "rerank" | "autoMerge">;
  /** 拼装上下文的选项。 */
  format?: FormatOptions;
}

/**
 * 用一个 client + collection 造一个"可直接挂到 LLM 的检索工具"。
 *
 * 返回对象同时给出：
 *  - `anthropic` / `openai`：两家工具定义（放进各自 API 的 `tools`）；
 *  - `run(input)`：收到 tool_use 时调它，得到可回灌的 `content` + 溯源 `citations`/`hits`。
 *
 * ACL 由服务端按 API Key 强制，工具无法越权；`fixed.filter` 仅在 ACL 之上进一步缩域。
 */
export function makeSearchTool(
  client: FastsearchClient,
  collection: string,
  opts: MakeSearchToolOptions = {},
): {
  name: string;
  description: string;
  anthropic: AnthropicToolDef;
  openai: OpenAIToolDef;
  run: (input: SearchToolInput) => Promise<SearchToolResult>;
} {
  const name = opts.name ?? "search_knowledge_base";
  const description =
    opts.description ??
    "检索受控知识库，返回带页码与引用标记的相关片段。用于回答需事实依据的问题；" +
      "回答时引用 [n] 标记并保留来源。";
  const defaultTopK = opts.defaultTopK ?? 8;

  const run = async (input: SearchToolInput): Promise<SearchToolResult> => {
    const query = typeof input?.query === "string" ? input.query : "";
    if (!query.trim()) {
      return { content: "（空查询）", citations: [], hits: [] };
    }
    const { hits } = await client.search(collection, query, {
      mode: input.mode ?? "hybrid",
      topK: input.top_k ?? defaultTopK,
      highlight: true,
      filter: opts.fixed?.filter as Filter | undefined,
      fusion: opts.fixed?.fusion,
      rerank: opts.fixed?.rerank,
      autoMerge: opts.fixed?.autoMerge,
    });
    const ctx = formatHitsForLLM(hits, opts.format);
    const content = hits.length
      ? ctx.content
      : "未检索到相关内容。可改写查询或换关键词重试。";
    return { ...ctx, content, hits };
  };

  return {
    name,
    description,
    anthropic: { name, description, input_schema: SEARCH_INPUT_SCHEMA },
    openai: {
      type: "function",
      function: { name, description, parameters: SEARCH_INPUT_SCHEMA },
    },
    run,
  };
}

// ---- LangChain.js 风格检索器 ---------------------------------------------

/** LangChain.js `Document` 的最小等价物（鸭子兼容，零硬依赖）。 */
export interface RetrievedDocument {
  pageContent: string;
  metadata: Record<string, unknown>;
}

/** 进 metadata 的命中字段（page_content 单独取自高亮片段）。 */
const METADATA_KEYS = [
  "citation_id",
  "score",
  "bm25",
  "vector",
  "rerank",
  "doc_id",
  "chunk_id",
  "page",
  "bbox",
  "heading_path",
  "section_id",
  "merged_chunk_ids",
  "time",
  "media",
] as const;

/** 单条命中 → LangChain.js 风格 `Document`（pageContent 取高亮片段）。 */
export function hitToDocument(hit: Hit): RetrievedDocument {
  const metadata: Record<string, unknown> = {};
  const row = hit as unknown as Record<string, unknown>;
  for (const k of METADATA_KEYS) {
    if (k in row && row[k] !== undefined) {
      metadata[k] = row[k];
    }
  }
  return { pageContent: hitText(hit), metadata };
}

/**
 * LangChain.js 风格检索器（鸭子兼容：`getRelevantDocuments` + `invoke`）。
 * 包一个 client + 固定 collection 与检索参数；`invoke(query)` → `Document[]`，
 * 可直接接入 LCEL（`retriever.pipe(prompt).pipe(model)`）。ACL 服务端强制，无法越权。
 */
export class FastsearchRetriever {
  constructor(
    private readonly client: FastsearchClient,
    private readonly collection: string,
    private readonly options: SearchOptions & { topK?: number } = {},
  ) {}

  async getRelevantDocuments(query: string): Promise<RetrievedDocument[]> {
    const hits = await this.client.searchHits(this.collection, query, {
      mode: "hybrid",
      topK: 8,
      highlight: true,
      ...this.options,
    });
    return hits.map(hitToDocument);
  }

  /** LangChain Runnable 别名。 */
  async invoke(query: string): Promise<RetrievedDocument[]> {
    return this.getRelevantDocuments(query);
  }
}
