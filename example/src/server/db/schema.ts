import { sql } from "drizzle-orm";
import { integer, sqliteTable, text } from "drizzle-orm/sqlite-core";

// 文档登记表：真正的可检索内容在 fastsearch 引擎里（PG 才是真源），
// 这里只存"喂过哪些 doc / 切了多少块"的本地清单，给 UI 列表用。
export const documents = sqliteTable("documents", {
  id: text("id").primaryKey(),
  title: text("title").notNull(),
  source: text("source").notNull().default("paste"),
  collection: text("collection").notNull(),
  chunkCount: integer("chunk_count").notNull().default(0),
  charLen: integer("char_len").notNull().default(0),
  createdAt: integer("created_at", { mode: "timestamp_ms" })
    .notNull()
    .default(sql`(unixepoch() * 1000)`),
});

// chunk 正文本地缓存：fastsearch 负责排序+引用锚点，但 /v1/search 不回传整段正文。
// 这里按 citation_id 存一份正文，searchKnowledgeBase 工具据此把"内容"喂给 Agent。
// （检索真源仍是 fastsearch/PG；这只是给答案层取正文的便捷缓存。）
export const chunks = sqliteTable("chunks", {
  citationId: text("citation_id").primaryKey(),
  docId: text("doc_id").notNull(),
  chunkId: integer("chunk_id").notNull(),
  text: text("text").notNull(),
});

// 聊天历史：演示 Drizzle 写路径（agent 回合落库）。一个全局会话流，够 example 用。
export const messages = sqliteTable("messages", {
  id: text("id").primaryKey(),
  role: text("role", { enum: ["user", "assistant"] }).notNull(),
  content: text("content").notNull(),
  createdAt: integer("created_at", { mode: "timestamp_ms" })
    .notNull()
    .default(sql`(unixepoch() * 1000)`),
});

export type Document = typeof documents.$inferSelect;
export type NewDocument = typeof documents.$inferInsert;
export type Message = typeof messages.$inferSelect;
