import { deepseek } from "@ai-sdk/deepseek";
import { tool } from "ai";
import { z } from "zod";
import { makeSearchTool } from "fastsearch-client";
import { COLLECTION, fastsearch } from "./fastsearch.ts";

// DeepSeek（OpenAI 兼容）。默认 deepseek-v4-flash，支持工具调用；读 DEEPSEEK_API_KEY。
// 可用 KB_MODEL 覆盖（如 deepseek-chat / deepseek-reasoner）。
export const MODEL = process.env.KB_MODEL ?? "deepseek-v4-flash";
export const model = deepseek(MODEL);

export const SYSTEM_PROMPT = `你是一个知识库问答 Agent。回答用户问题前，先用 searchKnowledgeBase 工具检索知识库取证。

规则：
- 凡是答案依赖知识库内容（事实、数字、定义、具体条款）就必须先检索，别凭记忆答。
- 工具返回的 content 里每条片段前有 [n] 序号，citations 里给出每个 [n] 对应的 citation_id / page。
- 引用证据时在句末标注对应的 citation_id，形如 [kb:report.pdf:3]，方便用户回溯到原文 page/bbox。
- 检索不到相关内容时，如实说"知识库里没有找到相关内容"，不要编造。
- 用中文回答，简洁、直接、先给结论。
- 一个问题可以多次检索（换关键词、补查），直到拿到足够证据再作答。`;

// SDK 的 makeSearchTool 一次产出工具定义 + 可执行 run()：run() 内部走
// client.search(highlight=true) → formatHitsForLLM 拼成带 [n] 标记的可引用上下文。
// ACL 由服务端按 API Key 强制，工具无法越权。
const kbTool = makeSearchTool(fastsearch, COLLECTION, { defaultTopK: 8 });

// 把 SDK 工具接到 Vercel AI SDK 的 tool loop。保留工具名 searchKnowledgeBase
// 与前端 tool part 类型对齐；输出 hits 供 UI 展示"检索到的来源"。
export const tools = {
  searchKnowledgeBase: tool({
    description: kbTool.description,
    inputSchema: z.object({
      query: z.string().describe("检索查询，用户问题里的关键信息或同义改写"),
      topK: z
        .number()
        .int()
        .min(1)
        .max(20)
        .optional()
        .describe("返回命中条数，默认 8"),
    }),
    execute: async ({ query, topK }) => {
      const out = await kbTool.run({ query, top_k: topK });
      return {
        // 带 [n] 标记的可引用上下文 + 平行的 marker→citation_id 来源表，供模型作答取证。
        content: out.content,
        citations: out.citations,
        // 给前端展示来源用（Chat.tsx 读 output.hits[].citation_id）。
        hits: out.hits.map((h) => ({
          citation_id: h.citation_id,
          score: Number(h.score.toFixed(4)),
          page: h.page,
          heading_path: h.heading_path,
        })),
      };
    },
  }),
};
