// 过滤构造器：用函数拼 Filter AST，避免手写判别键拼错、并获得类型检查。
//
//   import { f } from "fastsearch-client";
//   const filter = f.and(
//     f.eq("kind", "table"),
//     f.gte("page", 10),
//     f.in("doc_id", ["a.pdf", "b.pdf"]),
//   );
//   await c.search("kb", "毛利率", { filter });

import type { FieldValue, Filter } from "./types.js";

/** 过滤构造器命名空间。每个函数返回一个 {@link Filter} 节点，可任意嵌套。 */
export const f = {
  and: (...clauses: Filter[]): Filter => ({ and: clauses }),
  or: (...clauses: Filter[]): Filter => ({ or: clauses }),
  not: (clause: Filter): Filter => ({ not: clause }),
  eq: (field: string, value: FieldValue): Filter => ({ eq: [field, value] }),
  ne: (field: string, value: FieldValue): Filter => ({ ne: [field, value] }),
  gt: (field: string, value: FieldValue): Filter => ({ gt: [field, value] }),
  gte: (field: string, value: FieldValue): Filter => ({ gte: [field, value] }),
  lt: (field: string, value: FieldValue): Filter => ({ lt: [field, value] }),
  lte: (field: string, value: FieldValue): Filter => ({ lte: [field, value] }),
  in: (field: string, values: FieldValue[]): Filter => ({ in: [field, values] }),
  exists: (field: string): Filter => ({ exists: field }),
  /** heading_path 以给定序列为前缀（按标题层级缩域）。 */
  headingPrefix: (...prefix: string[]): Filter => ({ heading_prefix: prefix }),
};
