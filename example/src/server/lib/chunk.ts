import type { IndexChunk } from "./fastsearch.ts";

const ZERO_BBOX = { x0: 0, y0: 0, x1: 0, y1: 0 };

// 朴素切块：按段落聚合到 ~目标字符数。真实管线用 docparse 输出（带 page/bbox/heading_path），
// 这里粘贴纯文本，page 统一给 1、bbox 给 0——足够端到端演示检索与引用。
//
// ⚠️ **这段将被 SDK 的 `chunkText` 取代**（KB-1.4）：同样的算法已经收进 fastsearch-client
// 的 `src/ingest.ts`，还多了 markdown 标题 → heading_path 与 overlap。
// 本例引的是**已发布的** npm 包（package.json 里 `^0.2.0`），而 helper 在 0.3 才有 ——
// 等 SDK 发版后把本文件删掉、改 `import { chunkText } from "fastsearch-client"` 即可。
// 接入方式见 docs/接入文档摄取.md。
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
