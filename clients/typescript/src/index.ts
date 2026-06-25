// fastsearch TypeScript 客户端（零依赖，使用全局 fetch；Node 18+ / 浏览器）。
//
// 封装 fastsearch-server 的 REST API。ACL 由服务端按 API Key 强制，客户端无法越权。
//
// 用法：
//   import { FastsearchClient } from "fastsearch-client";
//   const c = new FastsearchClient("http://127.0.0.1:8642", "dev");
//   await c.index("kb", "report.pdf", chunks);   // docparse chunk 列表
//   const hits = await c.search("kb", "毛利率", { topK: 10 });
//   hits.forEach(h => console.log(h.citation_id, h.page, h.bbox));

export interface BBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
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
}

export interface SearchOptions {
  mode?: "keyword" | "vector" | "hybrid";
  topK?: number;
  filter?: unknown;
  vector?: number[];
}

export class FastsearchError extends Error {}

export class FastsearchClient {
  constructor(
    private baseUrl: string,
    private apiKey: string,
  ) {
    this.baseUrl = baseUrl.replace(/\/+$/, "");
  }

  private async post(path: string, body: unknown): Promise<any> {
    const resp = await fetch(this.baseUrl + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-API-Key": this.apiKey,
      },
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      const detail = await resp.text();
      throw new FastsearchError(`HTTP ${resp.status}: ${detail}`);
    }
    return resp.json();
  }

  /** 灌入一个 doc 的 chunks（doc 级替换）。返回灌入条数。 */
  async index(
    collection: string,
    docId: string,
    chunks: Record<string, unknown>[],
  ): Promise<number> {
    const mapped = chunks.map((ch) => {
      const c: Record<string, unknown> = { ...ch };
      if (c.doc_id === undefined) c.doc_id = docId;
      if (c.chunk_id === undefined && c.id !== undefined) {
        c.chunk_id = c.id;
        delete c.id;
      }
      if (c.acl === undefined) c.acl = ["public"];
      return c;
    });
    const out = await this.post("/v1/index", {
      collection,
      doc_id: docId,
      chunks: mapped,
    });
    return out.indexed ?? 0;
  }

  /** 检索。返回命中列表（带 page+bbox 引用）。 */
  async search(
    _collection: string,
    query: string,
    opts: SearchOptions = {},
  ): Promise<Hit[]> {
    const body: Record<string, unknown> = {
      query,
      mode: opts.mode ?? "hybrid",
      top_k: opts.topK ?? 20,
    };
    if (opts.filter !== undefined) body.filter = opts.filter;
    if (opts.vector !== undefined) body.vector = opts.vector;
    const out = await this.post("/v1/search", body);
    return out.hits ?? [];
  }
}
