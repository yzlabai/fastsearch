# spec · fastsearch-cli

> 模块 #7，依赖：fastsearch-core、fastsearch-engine。阶段 P1→P5。上游：[产品设计 §3.9/§4](../plans/2026-06-24-产品设计文档.md)。
> 状态：**开发中**。

## 1. 目的与范围

把引擎包成可运行的单二进制 CLI（四张脸之一）：

- `index`：读 docparse `-f chunks` 的 JSON/NDJSON → 灌入引擎（落盘 text 索引）→ commit。
- `search`：打开落盘索引 → 检索 → 表格/JSON 输出带引用命中。
- 索引目录持久化一份 `meta.json`（分词器等），保证 index/search 跨调用一致。

**不做**：REST/MCP（server）、向量持久化（MemVectorIndex 暂内存，故跨调用 search 为 keyword；hybrid 待向量落盘迭代）、复杂 filter 表达式解析（先给 `--kind/--page-min/--page-max` 简单标志）。

## 2. 命令

```
fastsearch index  --data <dir> --collection <c> --doc-id <id> [--tokenizer default|jieba] [INPUT|-]
fastsearch search --data <dir> --collection <c> --query <q> [--top-k N] [--kind K]
                  [--page-min N] [--page-max N] [--json]
```
- INPUT 为 docparse chunks 文件或 `-`/省略读 stdin。
- 输入格式：JSON 数组 或 NDJSON（每行一个 chunk）。docparse chunk 字段 `id`→`chunk_id`，`doc_id` 由 `--doc-id` 注入；`acl` 默认 `[public]`。

## 3. 公开接口（lib 部分，便于测试）

```rust
pub struct IndexOpts { pub data: PathBuf, pub collection, doc_id: String, pub tokenizer: TokenizerKind }
pub struct SearchOpts { pub data: PathBuf, pub collection, query: String, pub top_k: usize,
                        pub kind: Option<String>, pub page_min: Option<u32>, pub page_max: Option<u32> }
pub fn parse_chunks(bytes: &[u8], doc_id: &str) -> Result<Vec<Chunk>>;     // docparse→core::Chunk
pub fn cmd_index(opts, input: &[u8]) -> Result<usize>;                     // 返回灌入数
pub fn cmd_search(opts) -> Result<Vec<SearchHit>>;
pub fn build_filter(kind, page_min, page_max) -> Option<Filter>;
```

## 4. 行为规约

- `parse_chunks`：先试 JSON 数组，失败再按 NDJSON 逐行；坏行报错（行号）。每 chunk 注入 doc_id、缺省 acl/section_id/heading_path。
- `cmd_index`：open_or_create(data) → 按 doc_id 先 delete_by_doc 再灌入（doc 级替换语义）→ commit。
- `meta.json`：首次 index 写入 tokenizer；search 读它构造同构索引。
- `cmd_search`：keyword 模式（向量未落盘）；输出 citation_id/score/page/bbox/heading_path/截断文本。
- 健壮：坏 JSON 报错带上下文；空输入→0；data 目录自动建。

## 5. 依赖

`fastsearch-core`、`fastsearch-engine`、`clap`、`serde_json`、`serde`、`anyhow`；dev `tempfile`。

## 6. 测试用例

1. parse_chunks：JSON 数组 + NDJSON 都解析；id→chunk_id、doc_id 注入；坏行报错。
2. cmd_index + cmd_search：临时目录建库 → 查到、带引用（page/bbox/heading_path）。
3. doc 级替换：同 doc_id 再 index 覆盖。
4. build_filter：kind/page 范围生效（search 结果受限）。
5. 跨"调用"持久化：index 后新开 engine（同 data）search 仍命中。
6. 空输入 → 0；坏 JSON → Err。

## 7. 验收标准与状态

- [x] v1 完成：index/search 子命令 + docparse 解析（数组/NDJSON）+ 落盘 keyword 检索 + doc 级替换 + kind/page 过滤 + JSON/表格输出。5 单测绿 + **真二进制端到端验证**（index→search→引用→过滤全部正确）。clippy 净、fmt 净。
- [x] v1.1：`eval` 子命令（`cmd_eval`/`EvalOpts`）—— 对 golden 集跑真实检索算 nDCG/recall/MRR/precision，给 `--baseline` 则做回归门禁（掉点超 `--tol` 时非零退出，CI 可用）。复用 `eval::GoldenSet` + `engine::golden::run`（mode=Keyword，确定性）。+1 单测；**真二进制验证**：`fastsearch eval --golden …zh_finance.json --baseline …baseline.json` 输出指标且 `gate: OK`、exit 0。

**已知限制 / 下一迭代：**
- 跨调用为 keyword（向量索引未落盘）；hybrid 待向量持久化迭代。
- 过滤仅 `--kind/--page-min/--page-max` 简单标志；完整 filter DSL 走 REST/库 API。
- 真源应是 Postgres（CLI 当前直接落盘 text 索引演示；与 PG/CDC 串联待 server/sync 线缆层）。
