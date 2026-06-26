// fastsearch REST 客户端（薄封装；对齐 crates/fastsearch-server 的 /v1/* 接口）。
// 设计要点：ACL 由服务端按 API Key 强制，客户端无法越权——这里不传 acl 过滤。

const BASE_URL = (process.env.FASTSEARCH_URL ?? "http://127.0.0.1:8642").replace(
  /\/+$/,
  "",
);
const API_KEY = process.env.FASTSEARCH_API_KEY ?? "dev";
export const COLLECTION = process.env.FASTSEARCH_COLLECTION ?? "kb";

export interface BBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

// 落库的 chunk 形状，对齐 core::Chunk（snake_case）。
// 必填：doc_id / chunk_id / kind / text / page / bbox / char_len。
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

export interface Hit {
  citation_id: string;
  score: number;
  bm25: number | null;
  vector: number | null;
  doc_id: string;
  chunk_id: number;
  page: number;
  bbox: BBox;
  heading_path: string[];
  section_id: number;
  highlight?: string | null;
}

export class FastsearchError extends Error {}

async function post(path: string, body: unknown): Promise<any> {
  let resp: Response;
  try {
    resp = await fetch(BASE_URL + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-API-Key": API_KEY,
      },
      body: JSON.stringify(body),
    });
  } catch (cause) {
    throw new FastsearchError(
      `连不上 fastsearch（${BASE_URL}）。先起服务：FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" cargo run -p fastsearch-server --bin fastsearch-server`,
      { cause },
    );
  }
  if (!resp.ok) {
    throw new FastsearchError(`HTTP ${resp.status}: ${await resp.text()}`);
  }
  return resp.json();
}

/** doc 级替换写入一批 chunks，返回写入条数。 */
export async function indexDoc(
  docId: string,
  chunks: IndexChunk[],
): Promise<number> {
  const out = await post("/v1/index", {
    collection: COLLECTION,
    doc_id: docId,
    chunks,
  });
  return out.indexed ?? 0;
}

/** 混合检索，返回带 page+bbox 引用的命中列表。 */
export async function search(
  query: string,
  opts: { topK?: number; mode?: "keyword" | "vector" | "hybrid" } = {},
): Promise<Hit[]> {
  const out = await post("/v1/search", {
    query,
    mode: opts.mode ?? "hybrid",
    top_k: opts.topK ?? 8,
  });
  return out.hits ?? [];
}
