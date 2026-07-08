// fastsearch REST 客户端（零依赖，使用全局 fetch；Node 18+ / Deno / Bun / 浏览器）。
//
// 封装 fastsearch-server 的 REST API。ACL 由服务端按 API Key 强制，客户端无法越权。
//
//   import { FastsearchClient } from "fastsearch-client";
//   const c = new FastsearchClient({ baseUrl: "http://127.0.0.1:8642", apiKey: "dev" });
//   await c.index("kb", "report.pdf", chunks);
//   const { hits } = await c.search("kb", "毛利率", { topK: 10, highlight: true });

import { FastsearchError } from "./errors.js";
import type {
  Hit,
  ResolvedAsset,
  SearchOptions,
  SearchResponse,
} from "./types.js";

/** 客户端配置。 */
export interface ClientConfig {
  /** server 基址，如 `http://127.0.0.1:8642`。 */
  baseUrl: string;
  /** API Key（决定身份与 ACL）。 */
  apiKey: string;
  /** 默认单次请求超时（毫秒），默认 30000。 */
  timeoutMs?: number;
  /** 可重试错误（429/5xx/网络）的自动重试次数（指数退避），默认 0（不重试）。 */
  retries?: number;
  /** 注入自定义 fetch（测试 / 非标准运行时）；默认用全局 `fetch`。 */
  fetch?: typeof fetch;
}

/** index() 的可选参数。 */
export interface IndexOptions {
  /** 缺省 acl（chunk 自身未带 acl 时使用），默认 `["public"]`。 */
  defaultAcl?: string[];
  signal?: AbortSignal;
  timeoutMs?: number;
}

const DEFAULT_TIMEOUT_MS = 30_000;

/** fastsearch server 的 REST 客户端。线程安全、可复用单例。 */
export class FastsearchClient {
  private readonly baseUrl: string;
  private readonly apiKey: string;
  private readonly timeoutMs: number;
  private readonly retries: number;
  private readonly fetchImpl: typeof fetch;

  /**
   * 兼容两种构造形式：
   *   new FastsearchClient({ baseUrl, apiKey })           // 推荐
   *   new FastsearchClient("http://...:8642", "dev")      // 兼容旧用法
   */
  constructor(config: ClientConfig);
  constructor(baseUrl: string, apiKey: string);
  constructor(configOrUrl: ClientConfig | string, apiKey?: string) {
    const config: ClientConfig =
      typeof configOrUrl === "string"
        ? { baseUrl: configOrUrl, apiKey: apiKey ?? "" }
        : configOrUrl;
    this.baseUrl = config.baseUrl.replace(/\/+$/, "");
    this.apiKey = config.apiKey;
    this.timeoutMs = config.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.retries = config.retries ?? 0;
    const fImpl = config.fetch ?? globalThis.fetch;
    if (typeof fImpl !== "function") {
      throw new FastsearchError(
        "global fetch unavailable; pass `fetch` in config (Node <18?)",
      );
    }
    this.fetchImpl = fImpl;
  }

  // ---- HTTP 内核：超时 + 取消 + 可选重试 ----------------------------------

  private async request(
    method: "GET" | "POST" | "DELETE",
    path: string,
    body?: unknown,
    opts: { signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<Response> {
    const timeoutMs = opts.timeoutMs ?? this.timeoutMs;
    let lastErr: FastsearchError | undefined;
    for (let attempt = 0; attempt <= this.retries; attempt++) {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), timeoutMs);
      const onAbort = () => ctrl.abort();
      opts.signal?.addEventListener("abort", onAbort, { once: true });
      try {
        const resp = await this.fetchImpl(this.baseUrl + path, {
          method,
          headers: {
            "X-API-Key": this.apiKey,
            ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
          },
          body: body !== undefined ? JSON.stringify(body) : undefined,
          signal: ctrl.signal,
        });
        if (!resp.ok) {
          const detail = await resp.text().catch(() => "");
          const err = new FastsearchError(
            `HTTP ${resp.status}: ${detail || resp.statusText}`,
            resp.status,
            detail,
          );
          if (err.isRetryable && attempt < this.retries) {
            lastErr = err;
            await this.backoff(attempt);
            continue;
          }
          throw err;
        }
        return resp;
      } catch (e) {
        if (e instanceof FastsearchError) throw e;
        const aborted = opts.signal?.aborted;
        const msg = aborted
          ? "request aborted by caller"
          : `request failed: ${(e as Error).message}`;
        const err = new FastsearchError(msg, 0);
        if (!aborted && attempt < this.retries) {
          lastErr = err;
          await this.backoff(attempt);
          continue;
        }
        throw err;
      } finally {
        clearTimeout(timer);
        opts.signal?.removeEventListener("abort", onAbort);
      }
    }
    throw lastErr ?? new FastsearchError("request failed");
  }

  private backoff(attempt: number): Promise<void> {
    const ms = Math.min(2000, 100 * 2 ** attempt);
    return new Promise((r) => setTimeout(r, ms));
  }

  private async postJson<T>(
    path: string,
    body: unknown,
    opts: { signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<T> {
    const resp = await this.request("POST", path, body, opts);
    return (await resp.json()) as T;
  }

  // ---- 检索 --------------------------------------------------------------

  /**
   * 混合检索。返回命中列表（带 page+bbox 引用）+ 分面。
   *
   * `collection` 作用域**强制注入**为过滤子句（与 CLI 一致），多集合 server 上只返回本集合命中；
   * 与用户 `filter` 用 `and` 合并。ACL 由服务端按 API Key 强制，与此无关。
   * Agent 取上下文时建议 `{ highlight: true }`，让正文片段进入 `Hit.highlight`。
   */
  async search(
    collection: string,
    query: string,
    opts: SearchOptions = {},
  ): Promise<SearchResponse> {
    const body: Record<string, unknown> = {
      query,
      mode: opts.mode ?? "hybrid",
      top_k: opts.topK ?? 20,
    };
    if (opts.fusion !== undefined) body.fusion = opts.fusion;
    // collection 作用域：注入 Eq(collection) 过滤，与用户 filter `and` 合并（M23）。
    const collFilter = { eq: ["collection", collection] };
    body.filter =
      opts.filter !== undefined ? { and: [collFilter, opts.filter] } : collFilter;
    if (opts.vector !== undefined) body.vector = opts.vector;
    if (opts.queryImage !== undefined) body.query_image = opts.queryImage;
    if (opts.embedder !== undefined) body.embedder = opts.embedder;
    if (opts.candidates !== undefined) body.candidates = opts.candidates;
    if (opts.rerank !== undefined) body.rerank = opts.rerank;
    if (opts.autoMerge !== undefined) body.auto_merge = opts.autoMerge;
    if (opts.collapse !== undefined) body.collapse = opts.collapse;
    if (opts.searchAfter !== undefined) body.search_after = opts.searchAfter;
    if (opts.highlight !== undefined) body.highlight = opts.highlight;
    if (opts.facets !== undefined) body.facets = opts.facets;
    if (opts.explain !== undefined) body.explain = opts.explain;
    const out = await this.postJson<SearchResponse>("/v1/search", body, {
      signal: opts.signal,
      timeoutMs: opts.timeoutMs,
    });
    return { hits: out.hits ?? [], facets: out.facets ?? {} };
  }

  /** 便捷形态：只要命中数组（丢弃分面）。 */
  async searchHits(
    collection: string,
    query: string,
    opts: SearchOptions = {},
  ): Promise<Hit[]> {
    return (await this.search(collection, query, opts)).hits;
  }

  /**
   * 逐页遍历深分页：自动用上一页末条命中的 cursor 续取，直到不足一页或达 `maxPages`。
   * 适合 agent 做"全量扫读/汇总"。
   */
  async *paginate(
    collection: string,
    query: string,
    opts: SearchOptions & { maxPages?: number } = {},
  ): AsyncGenerator<Hit[], void, unknown> {
    const { maxPages = Infinity, ...base } = opts;
    const pageSize = base.topK ?? 20;
    let cursor = base.searchAfter;
    for (let page = 0; page < maxPages; page++) {
      const { hits } = await this.search(collection, query, {
        ...base,
        searchAfter: cursor,
      });
      if (hits.length === 0) return;
      yield hits;
      if (hits.length < pageSize) return;
      // 防死循环：末条无游标 / 游标未推进 → 停（否则会从第一页重头取）。
      const next = hits[hits.length - 1]?.cursor;
      if (next === undefined || next === cursor) return;
      cursor = next;
    }
  }

  /** more_like_this：按种子 `citation_id` 反查相似命中。 */
  async similar(
    citationId: string,
    opts: { topK?: number; signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<Hit[]> {
    const out = await this.postJson<{ hits?: Hit[] }>(
      "/v1/similar",
      { citation_id: citationId, top_k: opts.topK ?? 10 },
      { signal: opts.signal, timeoutMs: opts.timeoutMs },
    );
    return out.hits ?? [];
  }

  // ---- 资产 / 引用解析 ----------------------------------------------------

  /**
   * 批量把 citation_id 解析成可直接用的短时 URL（前端 `<img src>`）或跳原文 JSON。
   * ACL 强制：越权/不存在的 id 直接省略（不暴露存在性）。
   */
  async resolveAssets(
    citationIds: string[],
    opts: { signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<ResolvedAsset[]> {
    const out = await this.postJson<{ assets?: ResolvedAsset[] }>(
      "/v1/assets/resolve",
      { ids: citationIds },
      { signal: opts.signal, timeoutMs: opts.timeoutMs },
    );
    return out.assets ?? [];
  }

  /**
   * 取单个引用的 **inline 媒资字节**（按需从 PG 真源取）。
   * 返回 `null` 表示：不可见/不存在（404），或该引用是 DocRender（跳原文 JSON）而非 inline 字节。
   * DocRender / SignedUrl 类资产请用 {@link resolveAssets}（前者拿 page+bbox，后者拿短时 URL）。
   */
  async fetchAssetBytes(
    citationId: string,
    opts: { signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<{ bytes: Uint8Array; contentType: string } | null> {
    let resp: Response;
    try {
      resp = await this.request(
        "GET",
        `/v1/asset/${encodeURIComponent(citationId)}`,
        undefined,
        opts,
      );
    } catch (e) {
      if (e instanceof FastsearchError && e.status === 404) return null;
      throw e;
    }
    const ct = resp.headers.get("content-type") ?? "application/octet-stream";
    // DocRender 命中回的是 JSON（跳原文，非 inline 字节）——非本方法语义，引导用 resolveAssets。
    if (ct.includes("application/json")) return null;
    const buf = new Uint8Array(await resp.arrayBuffer());
    return { bytes: buf, contentType: ct };
  }

  // ---- 写入 --------------------------------------------------------------

  /**
   * 灌入一个 doc 的 chunks（doc 级替换）。返回灌入条数。
   *
   * `chunks` 为 docparse chunk（含 id/kind/text/page/bbox/...）；本方法补 doc_id、
   * 映射 id→chunk_id，acl 默认 `["public"]`（可经 `opts.defaultAcl` 覆盖）。
   */
  async index(
    collection: string,
    docId: string,
    chunks: Record<string, unknown>[],
    opts: IndexOptions = {},
  ): Promise<number> {
    const defaultAcl = opts.defaultAcl ?? ["public"];
    const mapped = chunks.map((ch) => {
      const c: Record<string, unknown> = { ...ch };
      if (c.doc_id === undefined) c.doc_id = docId;
      if (c.chunk_id === undefined && c.id !== undefined) {
        c.chunk_id = c.id;
        delete c.id;
      }
      if (c.acl === undefined) c.acl = defaultAcl;
      return c;
    });
    const out = await this.postJson<{ indexed?: number }>(
      "/v1/index",
      { collection, doc_id: docId, chunks: mapped },
      { signal: opts.signal, timeoutMs: opts.timeoutMs },
    );
    return out.indexed ?? 0;
  }

  /**
   * 删除一个 doc（真源 PG + 派生索引 + 关联对象）。
   * ACL 强制：不可见/不存在抛 404 FastsearchError（不暴露存在性）。
   */
  async deleteDoc(
    collection: string,
    docId: string,
    opts: { signal?: AbortSignal; timeoutMs?: number } = {},
  ): Promise<{
    deleted: boolean;
    pg_deleted?: number;
    objects_deleted?: number;
    object_errors?: string[];
  }> {
    // doc_id 可含 `/`（server 通配段）：按段编码、保留斜杠。
    const docPath = docId.split("/").map(encodeURIComponent).join("/");
    const resp = await this.request(
      "DELETE",
      `/v1/docs/${encodeURIComponent(collection)}/${docPath}`,
      undefined,
      opts,
    );
    return (await resp.json()) as {
      deleted: boolean;
      pg_deleted?: number;
      objects_deleted?: number;
      object_errors?: string[];
    };
  }

  // ---- 健康/契约 ---------------------------------------------------------

  /** 存活探针（无需鉴权）。server 在线返回 true。 */
  async health(opts: { timeoutMs?: number } = {}): Promise<boolean> {
    try {
      const resp = await this.request("GET", "/healthz", undefined, {
        timeoutMs: opts.timeoutMs ?? 5_000,
      });
      return resp.ok;
    } catch {
      return false;
    }
  }

  /** 取 OpenAPI 3.0 契约（手写、随 API 演进维护）。 */
  async openapi(opts: { signal?: AbortSignal } = {}): Promise<unknown> {
    const resp = await this.request("GET", "/openapi.json", undefined, opts);
    return resp.json();
  }
}
