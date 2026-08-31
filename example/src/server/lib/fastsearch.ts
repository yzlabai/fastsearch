// fastsearch 接入：直接复用仓库内当前 SDK `fastsearch-client`（零依赖、全局 fetch）。
// 本例不再手写 REST 客户端——index/search/工具定义/RAG 拼装全走 SDK。
// ACL 由服务端按 API Key 强制，客户端无法越权，所以这里不传 acl 过滤。

import { FastsearchClient, FastsearchError, type Chunk } from "fastsearch-client";

export { FastsearchError };

export const COLLECTION = process.env.FASTSEARCH_COLLECTION ?? "kb";

// 单例：线程安全、可复用。baseUrl/apiKey 走 .env（见 .env.example）。
export const fastsearch = new FastsearchClient({
  baseUrl: process.env.FASTSEARCH_URL ?? "http://127.0.0.1:8642",
  apiKey: process.env.FASTSEARCH_API_KEY ?? "dev",
  retries: 2,
});

/** doc 级替换写入一批 chunks，返回写入条数（薄封装 SDK，便于路由层调用）。 */
export async function indexDoc(docId: string, chunks: Chunk[]): Promise<number> {
  return fastsearch.index(COLLECTION, docId, chunks);
}
