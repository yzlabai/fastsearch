# 知识库 Agent 例子（Hono · Drizzle/SQLite · AI SDK · Vite/React/shadcn）

[English](./README.md) | 简体中文

一个端到端的"知识库 Agent"小应用，检索后端用本仓库的 **fastsearch** REST 引擎。

```
浏览器 (Vite/React/shadcn 聊天 UI)
   │  /api  →  Hono 服务 (:8787)
   │            ├─ /api/chat       Vercel AI SDK · streamText + 工具循环 (DeepSeek)
   │            │      └─ 工具 searchKnowledgeBase ──→ fastsearch /v1/search
   │            └─ /api/documents  切块 ──→ fastsearch /v1/index   + 登记到 SQLite(Drizzle)
   ▼
fastsearch-server (:8642)  ← 混合检索引擎（本仓库 Rust crate）
```

- **Hono**：后端 HTTP（聊天流式接口 + 文档喂入接口）。
- **SQLite + Drizzle**：本地存"文档清单"和"聊天历史"（可检索内容的真源在 fastsearch/PG，这里只是清单）。
- **Vercel AI SDK**：Agent 主体——`streamText` 跑工具循环，默认 **DeepSeek（deepseek-v4-flash）**；`searchKnowledgeBase` 工具调 fastsearch 取证。
- **Vite + React + shadcn/ui**：聊天界面 + 文档喂入面板，回答带可回溯的 `citation_id`。

> 这是给 fastsearch 配套的 *示例*，目的是展示"怎么在 fastsearch 上搭一个 RAG Agent"。

## 前置

- Node ≥ 20.12（用到 `process.loadEnvFile`）
- 一把 DeepSeek API key（[platform.deepseek.com](https://platform.deepseek.com)）
- 仓库根目录能 `cargo run`（起 fastsearch-server）

## 跑起来（3 个终端）

**① 起 fastsearch 检索引擎**（在仓库根目录，不是 example/）

```bash
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" \
  cargo run -p fastsearch-server --bin fastsearch-server
# 监听 :8642，API Key = dev。没配嵌入后端 → 纯关键词(BM25)模式，足够本例。
```

**② 装依赖 + 配 .env**（在 example/）

```bash
cd example
npm install
cp .env.example .env
# 编辑 .env，至少填 DEEPSEEK_API_KEY
```

**③ 起前后端**

```bash
npm run dev
# Hono 后端 http://127.0.0.1:8787
# 前端       http://127.0.0.1:5173   ← 打开它
```

打开 5173：左边粘一篇文档「喂入知识库」，右边就能提问，Agent 会先检索再带 `[kb:doc:chunk]` 引用作答。

## 端到端冒烟测试

栈起好后（①+③ 都在跑、`.env` 有 `DEEPSEEK_API_KEY`），新开一个终端：

```bash
npm run test:e2e
```

它打活着的服务，验证整条链路：健康检查 → 喂入文档(`/v1/index`+SQLite) → `/api/chat`
Agent 工具循环（调 `searchKnowledgeBase` → 拿 fastsearch 命中 → 带引用作答 → 正常结束）。
零依赖（只用 Node 内置 `fetch`），全绿退出码 0。脚本见 `test/smoke.mjs`。

## 想要语义检索（向量）？

给 fastsearch 配个嵌入后端（Ollama / OpenAI 兼容）即可，无需改本例代码——
`/api/chat` 的工具默认用 `mode: "hybrid"`，引擎侧配了嵌入就自动召回向量。
具体见仓库根 `CLAUDE.md` 与 `crates/fastsearch-embed`。

## 代码导览

```
src/server/
  index.ts              Hono 入口（serve :8787）
  env.ts                最先加载 .env
  db/schema.ts          Drizzle 表：documents / chunks / messages
  db/index.ts           better-sqlite3 + 启动兜底建表
  lib/fastsearch.ts     fastsearch REST 客户端（/v1/index, /v1/search）
  lib/chunk.ts          朴素切块（真实管线用 docparse）
  lib/agent.ts          模型 + 系统提示 + searchKnowledgeBase 工具
  routes/chat.ts        AI SDK streamText 工具循环，回合落库
  routes/documents.ts   喂入：切块→/v1/index→登记 SQLite；列表
src/web/
  App.tsx               双栏布局
  components/Chat.tsx          useChat 聊天 + 渲染工具来源
  components/DocumentsPanel.tsx 文档喂入 + 列表
  components/ui/*       shadcn 原语（button/card/input/textarea/badge）
test/
  smoke.mjs             端到端冒烟测试（npm run test:e2e）
```

## 不变量（沿用 fastsearch 的约束）

- **ACL 不可绕过**：检索/写入的权限由 fastsearch 服务端按 API Key 强制，客户端传不了也放宽不了 ACL。
- **PG 是真源、引擎索引是派生**：本例 SQLite 只存本地清单/缓存，不当权威数据。
- **诚实记账**：没配嵌入后端就是纯关键词模式——别把它当语义检索。
