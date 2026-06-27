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
fastsearch index     --data <dir> --collection <c> --doc-id <id> [--tokenizer default|jieba] [INPUT|-]
fastsearch index-dir --data <dir> --collection <c> [--tokenizer default|jieba] <DIR>
fastsearch search    --data <dir> --collection <c> --query <q> [--top-k N] [--kind K]
                     [--page-min N] [--page-max N] [--json]
fastsearch ingest    <FILE> --data <dir> --collection <c> --doc-id <id>  # 需 --features parse；多格式：PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML（按扩展名分发）
fastsearch eval      --golden <g.json> [--baseline <b.json>] [--tol] [--k]
```
- **`index-dir <DIR>`**：递归遍历文件夹下 `.md/.txt/.markdown/.text`，按文件做 doc 级灌入
  （`doc_id`=相对路径），markdown 标题切 `Heading` chunk + 维护 `heading_path`、空行分段。
  一个**不依赖 PDF/docparse 的"喂文件夹→检索"端到端闭环**。其余后缀忽略（PDF 走 `ingest`）。
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

- [x] **多格式摄取（2026-06-27，docparse 融合后）**：`fastsearch ingest <file>`（`--features parse`）经 docparse `DocumentParser` 注册表按扩展名分发，支持 **PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML + 图片**（无 ONNX）。`from_docparse_chunk` 适配 → 落盘索引 → 检索。+1 测试 `multiformat_dispatch`（md/html/csv）+ 实跑命中。
- [x] **OCR 摄取（2026-06-27，`--features parse-ocr`，真模型端到端验证）**：扫描件/图片无文本层 → docparse-img 解析（页标 `ScannedNoText`）→ `apply_ocr`（env `FASTSEARCH_OCR_MODELS` 指 PP-OCR ONNX 模型目录 → `PpOcrEnhancer` + `enhance::apply`）抽文本 → 索引可检索。重 tract/ONNX 仅此 feature（搜索热路径零依赖）。**真机验证**（ppocr-v5 det+rec+dict，omnidocbench 数据表页）：1/1 页增强→9 chunk（vs 不开仅 1 图 chunk）、OCR 文本 `Impedance/Reference/BLM18AG121SN1D` 索引后命中（2/8/6）。+1 env-gated 测试 `ocr_end_to_end_gated`（真模型 80s 绿）；模型不进仓（待运行验证策略）。

- [x] **表格结构识别（2026-06-28，`--features parse-tables`，非 VLM 确定性 ONNX）**：解析检测出的表格区域（`Element::Table`）→ docparse-raster（**纯 Rust hayro，无 pdfium**）从源 PDF 栅格化裁剪 → `UniRec`（ONNX，对应 docparse-cli `--unirec`，**非** `--vlm-tables`）重识别为结构化 HTML 表格 → 替换。env `FASTSEARCH_UNIREC_MODELS`。+1 env-gated 测试 `tables_refine_gated`（真模型路径端到端：lorem.pdf 0 表快速验证 load+refine 链路）。**注**：UniRec 是 2000-token 自回归解码，**CPU 上单复杂表耗时数分钟**，大批量建议 GPU。

**已知限制 / 下一迭代：**
- OCR/UniRec 模型需运行时下载（`docparse-rs/scripts/fetch-models.sh`，CI 无模型则相关测试 skip）。UniRec 表格解码 CPU 慢（自回归）。
- **VLM**（自然图/图表**语义描述**，OpenAI 兼容 HTTP）= `parse-vlm` 下一迭代——需 VLM 服务（如 Ollama llava），非 `Enhancer` trait，需对 image chunk 自定义编排。**区别**：表格/公式/版面**结构**已有本地 ONNX 确定性路（无需 VLM）；VLM 仅补"自然图语义描述"。
- 公式（UniRec→LaTeX）/ layout 版面增强同 ONNX 路可后续接。
- 跨调用为 keyword（向量索引未落盘）；hybrid 待向量持久化迭代。
- 过滤仅 `--kind/--page-min/--page-max` 简单标志；完整 filter DSL 走 REST/库 API。
- 真源应是 Postgres（CLI 当前直接落盘 text 索引演示；与 PG/CDC 串联待 server/sync 线缆层）。
