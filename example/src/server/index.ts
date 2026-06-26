import "./env.ts"; // 必须最先：在其它模块求值前加载 .env
import { serve } from "@hono/node-server";
import { Hono } from "hono";

import { chatRoute } from "./routes/chat.ts";
import { documentsRoute } from "./routes/documents.ts";

const app = new Hono();

app.get("/api/health", (c) => c.json({ ok: true }));
app.route("/api", chatRoute);
app.route("/api", documentsRoute);

const port = Number(process.env.PORT ?? 8787);

if (!process.env.DEEPSEEK_API_KEY) {
  console.warn(
    "[warn] 未设置 DEEPSEEK_API_KEY —— /api/chat 会失败。复制 .env.example 到 .env 并填 key。",
  );
}

serve({ fetch: app.fetch, port }, ({ port }) => {
  console.log(`▸ KB agent server  http://127.0.0.1:${port}`);
  console.log(`▸ 前端 (vite)       http://127.0.0.1:5173`);
});
