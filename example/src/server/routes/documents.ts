import { Hono } from "hono";
import { desc, eq } from "drizzle-orm";
import { db, schema } from "../db/index.ts";
import { chunkText } from "../lib/chunk.ts";
import { COLLECTION, FastsearchError, indexDoc } from "../lib/fastsearch.ts";

export const documentsRoute = new Hono();

// 列出已喂入的文档（本地清单）。
documentsRoute.get("/documents", (c) => {
  const docs = db
    .select()
    .from(schema.documents)
    .orderBy(desc(schema.documents.createdAt))
    .all();
  return c.json({ documents: docs });
});

// 喂入一篇文档：切块 → 推到 fastsearch（doc 级替换）→ 登记到 SQLite。
documentsRoute.post("/documents", async (c) => {
  const body = await c.req.json<{ title?: string; text?: string }>();
  const title = (body.title ?? "").trim();
  const text = (body.text ?? "").trim();
  if (!title || !text) {
    return c.json({ error: "title 和 text 都不能为空" }, 400);
  }

  // doc_id 用标题派生的稳定 slug；重复 title 会触发 fastsearch 的 doc 级替换（幂等重灌）。
  const docId = title
    .toLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 80) || `doc-${Date.now()}`;

  const chunks = chunkText(docId, text);
  if (chunks.length === 0) {
    return c.json({ error: "切块为空（正文太短？）" }, 400);
  }

  try {
    const indexed = await indexDoc(docId, chunks);

    // 本地正文缓存做 doc 级替换（与 fastsearch 的 doc 级替换语义对齐）。
    db.delete(schema.chunks).where(eq(schema.chunks.docId, docId)).run();
    db.insert(schema.chunks)
      .values(
        chunks.map((ch) => ({
          citationId: `${COLLECTION}:${docId}:${ch.chunk_id}`,
          docId,
          chunkId: ch.chunk_id,
          text: ch.text,
        })),
      )
      .run();

    db.insert(schema.documents)
      .values({
        id: docId,
        title,
        source: "paste",
        collection: COLLECTION,
        chunkCount: indexed,
        charLen: text.length,
      })
      .onConflictDoUpdate({
        target: schema.documents.id,
        set: { title, chunkCount: indexed, charLen: text.length },
      })
      .run();

    return c.json({ id: docId, title, chunkCount: indexed });
  } catch (err) {
    if (err instanceof FastsearchError) {
      return c.json({ error: err.message }, 502);
    }
    throw err;
  }
});
