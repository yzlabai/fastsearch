import type { IndexChunk } from "./fastsearch.ts";

const ZERO_BBOX = { x0: 0, y0: 0, x1: 0, y1: 0 };

// 朴素切块：按段落聚合到 ~目标字符数。真实管线用 docparse 输出（带 page/bbox/heading_path），
// 这里粘贴纯文本，page 统一给 1、bbox 给 0——足够端到端演示检索与引用。
export function chunkText(
  docId: string,
  text: string,
  targetLen = 900,
): IndexChunk[] {
  const paragraphs = text
    .split(/\n\s*\n/)
    .map((p) => p.replace(/\s+\n/g, "\n").trim())
    .filter(Boolean);

  const chunks: IndexChunk[] = [];
  let buf = "";

  const flush = () => {
    const body = buf.trim();
    if (!body) return;
    chunks.push({
      doc_id: docId,
      chunk_id: chunks.length,
      kind: "paragraph",
      text: body,
      page: 1,
      bbox: ZERO_BBOX,
      heading_path: [],
      char_len: body.length,
      acl: ["public"],
    });
    buf = "";
  };

  for (const p of paragraphs) {
    if (buf && buf.length + p.length > targetLen) flush();
    buf = buf ? `${buf}\n\n${p}` : p;
    // 单段就超长：直接成块，避免无限增长。
    if (buf.length >= targetLen) flush();
  }
  flush();

  return chunks;
}
