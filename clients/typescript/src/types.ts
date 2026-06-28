// fastsearch REST 类型定义（与 fastsearch-server 的 OpenAPI 契约对齐）。
//
// 所有类型为纯数据，零运行时依赖。字段命名沿用服务端 JSON（snake_case），
// 客户端 API 入参用 camelCase（见 SearchOptions），由 client 负责映射。

/** 文档区域的归一化包围盒（用于引用深链/高亮）。 */
export interface BBox {
  x0: number;
  y0: number;
  x1: number;
  y1: number;
}

/** 命中携带的媒资引用（图/表/音视频片段等）。结构随后端演进，保留为开放对象。 */
export type Media = Record<string, unknown>;

/** 一条检索命中。`citation_id` = `collection:doc_id:chunk_id`，是溯源/取资产的主键。 */
export interface Hit {
  /** 溯源主键：`collection:doc_id:chunk_id`。喂给 resolveAssets / similar。 */
  citation_id: string;
  /** 融合后的最终排序分。 */
  score: number;
  /** BM25 关键词分量（仅 keyword/hybrid 命中有值）。 */
  bm25: number | null;
  /** 向量相似分量（仅 vector/hybrid 命中有值）。 */
  vector: number | null;
  /** 神经/LTR rerank 分（未启用 rerank 时为 null）。 */
  rerank: number | null;
  doc_id: string;
  chunk_id: number;
  /** 1-based 页码（源文档无分页则为 0）。 */
  page: number;
  bbox: BBox;
  /** 标题层级路径（面包屑），用于上下文定位。 */
  heading_path: string[];
  section_id: number;
  /** 高亮片段（仅 `highlight: true` 时回；否则 null）。当作 LLM 上下文的正文。 */
  highlight: string | null;
  /** auto-merge 命中时被合并进来的兄弟 chunk_id。 */
  merged_chunk_ids: number[];
  /** 时间锚点（字幕/音视频 chunk 的时间区间，秒）；无则 null。 */
  time?: unknown;
  /** 媒资引用；无则 null。 */
  media?: Media | null;
  /** 不透明深分页游标：把上一页末条命中的此值作为下次 `searchAfter` 即续取下一页。 */
  cursor: string;
}

/** 一个分面字段的取值分布。 */
export interface FacetValue {
  value: string;
  count: number;
}

/** `/v1/search` 完整响应（命中 + 分面）。 */
export interface SearchResponse {
  hits: Hit[];
  /** `{field: [{value, count}]}`；未请求分面则为空对象。 */
  facets: Record<string, FacetValue[]>;
}

/** 检索模式：纯关键词 / 纯向量 / 混合（默认）。 */
export type SearchMode = "keyword" | "vector" | "hybrid";

/** 融合策略（对位服务端 `core::Fusion`，`method` 作判别字段）。 */
export type Fusion =
  | { method: "rrf"; rank_constant?: number }
  | { method: "normalized"; semantic_ratio?: number }
  | { method: "weighted"; alpha?: number };

/** rerank 配置。 */
export interface RerankSpec {
  model: string;
  top_k?: number;
}

/** 分组折叠：每个分组键最多保留 `max_per_group` 条（防单文档刷屏）。 */
export interface Collapse {
  /** 当前支持 `doc_id` / `section_id`。 */
  field: "doc_id" | "section_id" | string;
  max_per_group?: number;
}

/** 标量字段值（用于 Filter 比较）。 */
export type FieldValue = boolean | number | string;

/**
 * 过滤 AST（对位服务端 `core::Filter`，可嵌套布尔）。
 * 建议用 {@link f} 构造器而非手写对象，避免拼错判别键。
 */
export type Filter =
  | { and: Filter[] }
  | { or: Filter[] }
  | { not: Filter }
  | { eq: [string, FieldValue] }
  | { ne: [string, FieldValue] }
  | { gt: [string, FieldValue] }
  | { gte: [string, FieldValue] }
  | { lt: [string, FieldValue] }
  | { lte: [string, FieldValue] }
  | { in: [string, FieldValue[]] }
  | { exists: string }
  | { heading_prefix: string[] };

/** `search()` 的可选参数（camelCase；client 映射到 REST 的 snake_case）。 */
export interface SearchOptions {
  /** 默认 `"hybrid"`。 */
  mode?: SearchMode;
  /** 融合策略；缺省走服务端默认（RRF k=60）。 */
  fusion?: Fusion;
  /** 结构化过滤（ACL 不在此，由服务端按 Key 强制注入）。 */
  filter?: Filter;
  /** 外部提供的查询向量；不传则服务端用配置的嵌入后端现算。 */
  vector?: number[];
  /**
   * 以图搜图（MM9）：查询图的原始字节，作 u8 数组（如 `[...new Uint8Array(buf)]`）。
   * 设置后服务端用支持图像的后端嵌成查询向量；与 `vector` 二选一。
   */
  queryImage?: number[];
  /** 指定嵌入后端名（服务端配了多个时路由到具体一个）。 */
  embedder?: string;
  /** 召回候选窗口（深分页/折叠的上界），默认 150，须 ≥ topK。 */
  candidates?: number;
  /** 返回条数，默认 20。 */
  topK?: number;
  /** rerank 配置（默认不 rerank）。 */
  rerank?: RerankSpec;
  /** auto-merge 相邻同段 chunk，默认 false。 */
  autoMerge?: boolean;
  /** 分组折叠，默认不折叠。 */
  collapse?: Collapse;
  /** 深分页游标，取自上一页末条命中的 `cursor`。 */
  searchAfter?: string;
  /** 回高亮片段（进入 Hit.highlight），默认 false。Agent 取上下文应设 true。 */
  highlight?: boolean;
  /** 请求分面的字段（当前支持 `kind` / `doc_id`）。 */
  facets?: string[];
  /** 让服务端附带打分解释（调试用），默认 false。 */
  explain?: boolean;
  /** 覆盖单次请求超时（毫秒）；不传走 client 默认。 */
  timeoutMs?: number;
  /** 取消信号（与 timeoutMs 二选一或叠加）。 */
  signal?: AbortSignal;
}

/** resolveAssets 返回的单条资产。`type` 区分取用方式。 */
export type ResolvedAsset =
  | {
      citation_id: string;
      type: "inline";
      /** 短时签名 URL，可直接作 `<img src>`；服务端未配签名器时缺省、改返 `error`。 */
      url?: string;
      expires_s?: number;
      media_type?: string | null;
      error?: string;
    }
  | {
      citation_id: string;
      type: "object";
      url: string;
      expires_s: number;
    }
  | {
      citation_id: string;
      type: "doc_render";
      doc_id: string;
      page: number;
      bbox: BBox;
      media_type?: string | null;
    };
