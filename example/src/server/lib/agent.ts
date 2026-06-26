import { deepseek } from "@ai-sdk/deepseek";
import { tool } from "ai";
import { inArray } from "drizzle-orm";
import { z } from "zod";
import { db, schema } from "../db/index.ts";
import { search } from "./fastsearch.ts";

// DeepSeek（OpenAI 兼容）。deepseek-chat = V3，支持工具调用；读 DEEPSEEK_API_KEY。
// 想要推理模型可换 deepseek-reasoner（注意其工具调用支持较弱）。
export const MODEL = process.env.KB_MODEL ?? "deepseek-chat";
export const model = deepseek(MODEL);

export const SYSTEM_PROMPT = `你是一个知识库问答 Agent。回答用户问题前，先用 searchKnowledgeBase 工具检索知识库取证。

规则：
- 凡是答案依赖知识库内容（事实、数字、定义、具体条款）就必须先检索，别凭记忆答。
- 检索不到相关内容时，如实说"知识库里没有找到相关内容"，不要编造。
- 用中文回答，简洁、直接、先给结论。
- 引用证据时在句末标注来源 citation_id，形如 [kb:report.pdf:3]，方便用户回溯到原文 page/bbox。
- 一个问题可以多次检索（换关键词、补查），直到拿到足够证据再作答。`;

// 给 Agent 的检索工具：服务端调用 fastsearch /v1/search，回收裁剪过的命中（带引用锚点）。
export const tools = {
  searchKnowledgeBase: tool({
    description:
      "在知识库里做混合检索（关键词∥向量）。用于回答任何依赖知识库内容的问题。返回命中片段及其 citation_id / page / 高亮。",
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
      const hits = await search(query, { topK: topK ?? 8 });

      // fastsearch 负责排序+引用锚点；正文从本地缓存按 citation_id 取，喂给 Agent。
      const ids = hits.map((h) => h.citation_id);
      const texts = ids.length
        ? new Map(
            db
              .select()
              .from(schema.chunks)
              .where(inArray(schema.chunks.citationId, ids))
              .all()
              .map((r) => [r.citationId, r.text] as const),
          )
        : new Map<string, string>();

      return {
        count: hits.length,
        hits: hits.map((h) => ({
          citation_id: h.citation_id,
          score: Number(h.score.toFixed(4)),
          page: h.page,
          heading_path: h.heading_path,
          // 优先 fastsearch 高亮，回落到本地缓存的整段正文。
          snippet: h.highlight ?? texts.get(h.citation_id) ?? null,
        })),
      };
    },
  }),
};
