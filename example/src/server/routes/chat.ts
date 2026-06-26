import { randomUUID } from "node:crypto";
import { Hono } from "hono";
import {
  convertToModelMessages,
  stepCountIs,
  streamText,
  type UIMessage,
} from "ai";
import { db, schema } from "../db/index.ts";
import { model, SYSTEM_PROMPT, tools } from "../lib/agent.ts";

export const chatRoute = new Hono();

function lastUserText(messages: UIMessage[]): string {
  const last = [...messages].reverse().find((m) => m.role === "user");
  if (!last) return "";
  return last.parts
    .filter((p): p is { type: "text"; text: string } => p.type === "text")
    .map((p) => p.text)
    .join("\n")
    .trim();
}

chatRoute.post("/chat", async (c) => {
  const { messages } = await c.req.json<{ messages: UIMessage[] }>();

  // 落库用户这一轮（演示 Drizzle 写路径）。
  const userText = lastUserText(messages);
  if (userText) {
    db.insert(schema.messages)
      .values({ id: randomUUID(), role: "user", content: userText })
      .run();
  }

  const result = streamText({
    model,
    system: SYSTEM_PROMPT,
    messages: await convertToModelMessages(messages),
    tools,
    // Agent 循环：最多 6 步（检索→读结果→再检索→…→作答）。
    stopWhen: stepCountIs(6),
    onError: ({ error }) => {
      console.error("[chat] streamText error:", error);
    },
    onFinish: ({ text }) => {
      if (text.trim()) {
        db.insert(schema.messages)
          .values({ id: randomUUID(), role: "assistant", content: text })
          .run();
      }
    },
  });

  // 把工具调用也回传给前端（tool parts），便于 UI 展示"检索到的来源"。
  return result.toUIMessageStreamResponse({ sendReasoning: false });
});
