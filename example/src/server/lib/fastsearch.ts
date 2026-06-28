// fastsearch 接入：直接复用已发布的 SDK `fastsearch-client`（零依赖、全局 fetch）。
// 本例不再手写 REST 客户端——index/search/工具定义/RAG 拼装全走 SDK。
// ACL 由服务端按 API Key 强制，客户端无法越权，所以这里不传 acl 过滤。

import { FastsearchClient, FastsearchError } from "fastsearch-client";

export { FastsearchError };

export const COLLECTION = process.env.FASTSEARCH_COLLECTION ?? "kb";

// 单例：线程安全、可复用。baseUrl/apiKey 走 .env（见 .env.example）。
export const fastsearch = new FastsearchClient({
  baseUrl: process.env.FASTSEARCH_URL ?? "http://127.0.0.1:8642",
  apiKey: process.env.FASTSEARCH_API_KEY ?? "dev",
  retries: 2,
});

// ---- 本例本地的 chunk 形状 ----------------------------------------------
// 仅给朴素切块器（lib/chunk.ts）做类型用；对齐 docparse / core::Chunk 的 snake_case。
// 真实管线直接喂 docparse 输出，无需手写这个类型。

export interface BBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

export interface IndexChunk {
  doc_id: string;
  chunk_id: number;
  kind:
    | "heading"
    | "paragraph"
    | "table"
    | "code"
    | "list_item"
    | "image"
    | "audio"
    | "video";
  text: string;
  page: number;
  bbox: BBox;
  heading_path?: string[];
  section_id?: number;
  char_len: number;
  acl?: string[];
}

/** doc 级替换写入一批 chunks，返回写入条数（薄封装 SDK，便于路由层调用）。 */
export async function indexDoc(
  docId: string,
  chunks: IndexChunk[],
): Promise<number> {
  return fastsearch.index(COLLECTION, docId, chunks as unknown as Record<
    string,
    unknown
  >[]);
}
