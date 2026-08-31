// 文档摄取适配层：把**别的工具产出的东西**变成 fastsearch 能收的 chunk。
//
// 为什么需要它：fastsearch 的契约是"调用方拥有解析与分块，引擎拥有检索"
// （见仓内 docs/governance 的职责边界 ADR）。既然把解析推给调用方，就得把**契约**交出去——
// 否则调用方只能靠读 Rust 适配器源码来拼 JSON。本模块就是那份契约的可执行版本。
//
//   import { chunksFromDocparse, chunkText } from "fastsearch-client";
//   // ① 用任意解析器（如 docparse 的 REST 服务）拿到 chunks，再适配：
//   const chunks = chunksFromDocparse(docparseChunks, { docId: "report.pdf" });
//   await client.index("kb", "report.pdf", chunks);
//   // ② 纯文本/markdown：直接切
//   await client.index("kb", "notes.md", chunkText(text, { docId: "notes.md" }));

import type { BBox, Chunk } from "./types.js";

/** docparse `chunk` 输出里与本适配相关的字段（其余字段忽略）。 */
export interface DocparseChunk {
  /** docparse 用 `id`，fastsearch 用 `chunk_id` —— **唯一的字段重命名**。 */
  id: number;
  kind: string;
  text: string;
  page: number;
  bbox: BBox;
  heading_path?: string[];
  section_id?: number;
  char_len?: number;
  /** 仅 `kind:"image"` 的 chunk 有。 */
  image?: {
    /** 导出到磁盘/对象存储的路径（docparse `--image-dir`）。 */
    file?: string;
    /** 内嵌图片的 **base64**（docparse `--image-embed` / REST `?images=embedded`）。 */
    data_base64?: string;
    media_type?: string;
    caption?: string;
    caption_source?: string;
  };
}

export interface FromDocparseOptions {
  docId: string;
  /** 写入时附加的 ACL 标签。**通常不要设**：服务端会按 API Key 覆盖它。 */
  acl?: string[];
  /** 保留 `heading_path`/`section_id`（默认 true）。 */
  keepStructure?: boolean;
}

/** base64 → 字节数组。**不是可选步骤**：见 `chunksFromDocparse` 的说明。 */
function base64ToBytes(b64: string): number[] {
  // 允许 data URL 形态（`data:image/png;base64,xxxx`）。
  const raw = b64.includes(",") ? b64.slice(b64.lastIndexOf(",") + 1) : b64;
  const nodeBuffer = (
    globalThis as typeof globalThis & {
      Buffer?: {
        from(input: string, encoding: string): { toString(encoding: string): string };
      };
    }
  ).Buffer;
  if (typeof atob !== "function" && nodeBuffer === undefined) {
    throw new Error("base64 decoder unavailable in this runtime");
  }
  const bin =
    typeof atob === "function"
      ? atob(raw)
      : nodeBuffer!.from(raw, "base64").toString("binary");
  const out = new Array<number>(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * docparse chunk → fastsearch chunk。
 *
 * 两侧 schema 刻意同构，**只有三处要动**：
 * 1. `id` → `chunk_id`（唯一的重命名）；
 * 2. `image` → `media` 三态 —— `data_base64` ⇒ `{kind:"inline"}` + `media_bytes`；
 *    `file` ⇒ `{kind:"object", uri}`；两者皆无 ⇒ `{kind:"doc_region", page, bbox}`（只跳原文位置）；
 * 3. 注入 `doc_id`（docparse 的 chunk 不带它）。
 *
 * ⚠️ **`media_bytes` 是字节数组，不是 base64 字符串**。这是实测过的线缆契约：
 * 传 base64 字符串会被服务端以 `invalid type: string …, expected a sequence` 拒绝。
 * docparse 给的是 base64，所以这一步解码**必须做**——这正是手写 JSON 最容易踩的坑。
 */
export function chunksFromDocparse(
  chunks: DocparseChunk[],
  opts: FromDocparseOptions,
): Chunk[] {
  const { docId, acl, keepStructure = true } = opts;
  return chunks.map((d) => {
    const out: Chunk = {
      doc_id: docId,
      chunk_id: d.id,
      kind: d.kind,
      text: d.text,
      page: d.page,
      bbox: d.bbox,
      char_len: d.char_len ?? [...d.text].length,
    };
    if (keepStructure) {
      if (d.heading_path) out.heading_path = d.heading_path;
      if (d.section_id !== undefined) out.section_id = d.section_id;
    }
    if (acl) out.acl = acl;
    if (d.image) {
      const mediaType = d.image.media_type;
      if (d.image.data_base64) {
        out.media = {
          asset: { kind: "inline" },
          media_type: mediaType,
          region: d.bbox,
          caption_source: d.image.caption_source,
        };
        out.media_bytes = base64ToBytes(d.image.data_base64);
      } else if (d.image.file) {
        out.media = {
          asset: { kind: "object", uri: d.image.file },
          media_type: mediaType,
          region: d.bbox,
          caption_source: d.image.caption_source,
        };
      } else {
        // 没有字节也没有对象：只能指回原文位置（读者仍可跳到该页那一块）。
        out.media = {
          asset: { kind: "doc_region", page: d.page, bbox: d.bbox },
          media_type: mediaType,
          region: d.bbox,
          caption_source: d.image.caption_source,
        };
      }
    }
    return out;
  });
}

export interface ChunkTextOptions {
  docId: string;
  /** 目标块长（字符）。累计到该长度就断开，默认 900。 */
  targetChars?: number;
  /** 相邻块的重叠字符数，默认 0。 */
  overlap?: number;
  acl?: string[];
}

const ZERO_BBOX: BBox = { x0: 0, y0: 0, x1: 0, y1: 0 };

/**
 * 纯文本 / markdown 切块：**空行分段**聚合到目标长度；markdown 标题（`# …`）维护
 * `heading_path` 并自成一个 `heading` chunk。
 *
 * **坐标是占位的**：`page=1`、`bbox` 全 0 —— 纯文本没有版面。要真正的 page/bbox
 * （从而让 `resolve_citation` 能在原文里高亮），得用真正的解析器，见 `chunksFromDocparse`。
 * 这一点必须说清楚，否则调用方会以为自己拿到了可溯源的坐标。
 */
export function chunkText(text: string, opts: ChunkTextOptions): Chunk[] {
  const { docId, targetChars = 900, overlap = 0, acl } = opts;
  const chunks: Chunk[] = [];
  const path: Array<{ level: number; title: string }> = [];
  let buf: string[] = [];

  const push = (kind: string, body: string) => {
    if (!body) return;
    const c: Chunk = {
      doc_id: docId,
      chunk_id: chunks.length,
      kind,
      text: body,
      page: 1,
      bbox: ZERO_BBOX,
      heading_path: path.map((p) => p.title),
      char_len: [...body].length,
    };
    if (acl) c.acl = acl;
    chunks.push(c);
  };

  const flush = () => {
    const body = buf.join("\n").trim();
    buf = [];
    if (!body) return;
    push("paragraph", body);
    if (overlap > 0) {
      // 尾部重叠：下一块以上一块的末 `overlap` 个字符起头，避免答案跨块被切断。
      const tail = [...body].slice(-overlap).join("");
      if (tail) buf.push(tail);
    }
  };

  for (const line of text.split(/\r?\n/)) {
    const h = /^(#{1,6})\s+(.*\S)\s*$/.exec(line);
    if (h?.[1] && h[2]) {
      flush();
      const level = h[1].length;
      const title = h[2];
      while (path.length && (path[path.length - 1]?.level ?? 0) >= level) path.pop();
      path.push({ level, title });
      push("heading", title);
      continue;
    }
    if (!line.trim()) {
      if (buf.join("\n").trim().length >= targetChars) flush();
      else if (buf.length) buf.push("");
      continue;
    }
    buf.push(line);
    if (buf.join("\n").length >= targetChars) flush();
  }
  flush();
  return chunks.filter((c) => c.text.trim().length > 0);
}
