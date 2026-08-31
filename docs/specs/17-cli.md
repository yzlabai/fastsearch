# spec · fastsearch-cli

> 模块 #12，依赖：fastsearch-core（纯类型）、fastsearch-eval（纯指标）、ureq（HTTP）；`parse*` feature 下 + docparse。**不依赖 engine/text/vector**。上游：[产品设计 §3.9/§4](../plans/2026-06-24-产品设计文档.md)、[CLI 改为 REST 客户端设计](../plans/2026-06-28-CLI改为REST客户端设计.md)。
> 状态：**已落地**（2026-06-28 重构为**纯 REST 客户端**；2026-08-25 补图片字节闭环；2026-08-31 补可追溯分块 profile 与 geometry-safe overlap；见 §7）。

## 1. 目的与范围

`fastsearch` 二进制是 **server 的纯 REST 客户端**（四张脸之一，与 `clients/{python,ts}` 同模型）。**不嵌引擎**——检索/嵌入/落盘全归 server；CLI 只做：命令→端点映射 + **客户端侧分块/解析** + I/O。因此 CLI 自动获得 server 的**全套混合检索 + 真语义 + ACL**。

- `search`/`similar`：POST `/v1/search` / `/v1/similar`。
- `index`/`index-dir`/`ingest`：客户端分块/解析 → POST `/v1/index`（server 端嵌入+索引）。
- `eval`：语料 POST 入库 → 逐查询 POST `/v1/search` → 本地 `fastsearch-eval` 算指标 + 回归门禁。

**不做**：嵌入式离线引擎（已移除，2026-06-28——业界服务端检索产品 Typesense/Qdrant/Algolia/Meilisearch 的 CLI 皆为瘦客户端）；server 端文件解析（解析留客户端，守搜索热路径零 docparse）；复杂 filter DSL（先给 `--kind/--page-min/--page-max`）。

## 2. 命令

全局：`--server <URL>`（env `FASTSEARCH_SERVER`，默认 `http://localhost:8642`）、`--key <K>`（env `FASTSEARCH_KEY`，作 `Authorization: Bearer`）。

```
fastsearch search    --collection <c> --query <q> [--mode hybrid|keyword|vector] [--top-k N]
                     [--kind K] [--page-min N] [--page-max N] [--json]
fastsearch similar   --citation-id <cid> [--top-k N] [--json]
fastsearch index     --collection <c> --doc-id <id> [INPUT|-]      # docparse chunks JSON/NDJSON
fastsearch index-dir --collection <c> [--concurrency N]
                     [--chunk-profile P --chunk-profile-version V]
                     [--chunk-target N --chunk-overlap N] <DIR>
fastsearch ingest    <FILE> --collection <c> --doc-id <id> [--tenant T]
                     [--images object|inline|none]
                     [--chunk-profile P --chunk-profile-version V]
                     [--chunk-target N --chunk-overlap N --chunk-table-markdown]
fastsearch eval      --golden <g.json> [--baseline <b.json>] [--tol] [--k] [--mode]
```
- **`search`**：默认 `--mode hybrid`——server 有嵌入器则混合，否则自动退化关键词（不报错）。`--collection` 经 `filter: Eq("collection",…)` 限定作用域（collection 两端可过滤）。
- **`index-dir <DIR>`**：递归遍历 `.md/.txt/.markdown/.text`，每文件 `chunk_text` 切块（markdown 标题→`Heading` + `heading_path`、空行分段）→ POST `/v1/index`（`doc_id`=相对路径）。**有界并发**（`--concurrency`，默认 4，`std::thread::scope` + 原子游标/聚合，抵消单文件 POST 往返延迟）+ **进度输出 + 逐文件 continue-on-error**（有失败则退出码 1；计数确定、进度行可能交错）。"喂文件夹→检索"经 server → 反得**混合检索**。
- **`ingest`**：客户端 docparse 解析（`parse` feature，9 格式+图片；`parse-ocr`/`parse-tables` env 指模型目录；`parse-vlm` env 指 VLM 服务）→ 适配 chunks → POST `/v1/index`。
- **分块 profile（FS-204）**：`ingest` 默认 `docparse` v1/target 800，`index-dir` 默认 `fastsearch-text` v1/target 900；两者均校验 profile 非空、version > 0、target > 0、`overlap < target`。每条 chunk 写入 `metadata.chunking={chunker,profile,version,target_chars,overlap_chars,table_markdown}`。docparse overlap 只复用完整源 block，不跨 page/heading/table/image/code/list，因此 bbox 仍是真实源几何 union；profile 变化只影响新摄取，不隐式重切旧文档。
- **`ingest --images`**（KB-1.1，2026-08-25）：文档图片**原始字节**的去向。`object`（默认）→ 随 `/v1/index` 上传、server 落对象存储，`/v1/asset/{cid}` 由此签发短时 URL 吐回原图；`inline` → server 内联进 PG `bytea`（**需 server 配 `DATABASE_URL`**，且整本 PDF 的 base64 易撑爆 server 20MB `DefaultBodyLimit`，故须显式选）；`none` → 一个字节都不采（与本能力落地前逐字段一致，PDF 连 `decode_images` 都不开，不付内存代价）。**仅闭环 `Jpeg`/`Encoded` 两类**（PDF DCTDecode 直通 / DOCX·PPTX·HTML 媒体文件字节，零新依赖）；`Rgb8`/`Gray8` 裸位图**本迭代不支持**，如实标 `image_vector_status=missing_bytes` + stderr 计数，绝不伪装成已嵌入。
- INPUT 为 docparse chunks 文件或 `-`/省略读 stdin；JSON 数组 或 NDJSON。

## 3. 公开接口（lib 部分，便于测试）

```rust
pub struct Client { /* base, key */ }            // ureq 瘦封装；post(retry)；Authorization: Bearer
pub struct SearchOpts  { server, key, collection, query, mode: SearchMode, top_k, kind, page_min, page_max }
pub struct SimilarOpts { server, key, citation_id, top_k }
pub struct IndexOpts   { server, key, collection, doc_id, store_media: Option<StoreMedia> }
pub struct ChunkProfile { /* name/version/target/overlap/table；构造时校验 */ }
pub struct IngestOpts  { file, server, key, collection, doc_id, tenant, acl, images, chunk_profile }  // parse feature
pub enum   ImageBytes  { Object /*默认*/, Inline, None }   // 图片原始字节去向 → store_media
pub struct IndexDirOpts{ server, key, collection, concurrency, chunk_profile }  // 有界并发上传
pub struct EvalOpts    { server, key, golden, baseline, tol, k, mode }
pub fn parse_chunks(bytes, doc_id) -> Result<Vec<Chunk>>;   // docparse→core::Chunk（纯）
pub fn chunk_text(content, doc_id) -> Vec<Chunk>;            // md/txt 分块（纯）
pub fn chunk_text_with(content, doc_id, profile) -> Vec<Chunk>; // 显式 profile + provenance
pub fn build_filter(collection, kind, page_min, page_max) -> Filter;  // 必含 collection 作用域
pub fn cmd_search(opts) -> Result<Vec<Value>>;   // 返回 server hit 对象数组（原样透传）
pub fn cmd_similar(opts) -> Result<Vec<Value>>;
pub fn cmd_index(opts, input) -> Result<usize>;  // POST /v1/index，返回 indexed 数
pub fn cmd_index_dir(opts, root) -> Result<(usize, usize, usize)>;  // (成功, 失败, chunk 总数)
pub fn cmd_eval(opts) -> Result<(Metrics, Option<Result<(),String>>)>;
pub fn ingest::chunks_for_file(opts) -> Result<Vec<Chunk>>;  // 解析→增强→分块→适配（不碰网络，可离线单测）
pub fn ingest::cmd_ingest(opts) -> Result<usize>;            // = chunks_for_file + POST /v1/index
```

## 4. 行为规约

- `Client`：`--server`/`--key` 显式 > env > 默认 localhost:8642；非 2xx → 带状态码+body 报错；连接失败 → 友好提示（server 在跑吗）。`index` 走 `post_retry`（传输失败重试 3 次；4xx/5xx 确定性拒绝不重试）。
- `parse_chunks`/`chunk_text`：纯函数，客户端分块；坏行报错（行号）。
- `cmd_search`：构造 `core::SearchRequest`（含 collection/kind/page filter）→ POST → 取 `hits` 数组原样返回（`--json` 透传全部 server 字段，便于脚本/agent）。
- `cmd_index_dir`：确定性排序遍历；逐文件 POST，进度到 stderr，单文件失败不中断（计数）。
- `cmd_eval`：语料按 `doc_id` 分组 POST 入 golden 的 `collection` → 逐查询检索取 citation_id → `GlobalId::parse` → `evaluate` 算指标；`--baseline` → `assert_no_regression`。**会写入目标 server**（用专用/临时集合）。

## 5. 依赖

`fastsearch-core`（纯类型）、`fastsearch-eval`（纯指标）、`ureq`(json)、`clap`、`serde_json`、`serde`、`anyhow`；`parse*` feature + docparse-*；dev `tempfile`。**无 engine/text/vector**（`cargo tree` 校验）。

## 6. 测试用例

1. `parse_chunks`：JSON 数组 + NDJSON 解析；id→chunk_id、doc_id 注入。
2. `chunk_text`：markdown 标题→Heading、heading_path 累积、空行分段。
3. `build_filter`：必含 collection；+kind/page → And。
4. mock HTTP server：`cmd_search` 解析 hits、`cmd_index` 取 indexed 数、`cmd_index_dir` 喂文件夹多文件上传、500 错误上浮。
5. **图片字节闭环**（`tests/fixtures/`，无外部模型/服务/网络）：含图 PDF（DCTDecode JPEG + Flate 裸 RGB 位图）与含图 DOCX（内嵌 PNG）各一份 → `chunks_for_file` → `media_bytes` 与源图**逐字节相同**；`page`/`bbox`/`region` 不变；`Rgb8` 那张标 `missing_bytes` 且 asset 不得声称 Inline；`--images none` 与开启档 chunk 数/文本/坐标逐一相同且零 `media_bytes`/零状态写入。
6. （端到端，真 server，CI `cli-server-e2e` job）：起 fastsearch-server → `index-dir`(喂文件夹)→`search` 命中 / stdin `index`→`search` 命中 / `eval`(golden 入库→检索→nDCG=1) 闭环。
7. FS-204：docparse 完整 block overlap 的 bbox/page/section 回归；CLI text/profile 校验与真实 HTTP provenance；两组 target 对同一文件形成不同确定性边界。

## 7. 验收标准与状态

- [x] **重构为纯 REST 客户端（2026-06-28）**：删嵌入式引擎（engine/text 依赖移除，`cargo tree` 校验）；search/similar/index/index-dir/ingest/eval 全走 server REST；`--server`/`--key`(+env)；`ureq` HTTP。**喂文件夹保留**——改为客户端分块→POST，经 server 得混合检索。+7 客户端单测（纯函数 + mock HTTP：search/index/index-dir/错误上浮）；**真二进制端到端验证**：起 server（dev key）→ `index-dir`(2 文件 5 chunk) → `search "毛利率"` 命中、`--json` 全字段、stdin `index` + `search "alpha"` 命中。收口三绿。
- [x] **多格式摄取（`--features parse`）**：客户端 docparse 注册表分发 PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML + 图片 → 适配 → POST `/v1/index`。解析在客户端（守搜索热路径零 docparse + CI 门禁）。`multiformat_dispatch` 测试。
- [x] **OCR / 表格识别（`--features parse-ocr` / `parse-tables`）**：客户端解析期增强（env 指 ONNX 模型目录），抽出的文本/结构随 chunks 上传。真模型 env-gated 验证（见历史 devlog）。
- [x] **VLM 区域识别（`--features parse-vlm`，2026-07-27，代码落地／质量门待跑）**：表格区域经外部 OpenAI 兼容服务（vLLM/SGLang）重识别为 HTML 表；另设 `FASTSEARCH_LAYOUT_MODEL` 时加整页**区域级**转写。上游是 docparse 的 `RegionReader` 接缝——UniRec（进程内 ONNX）与 VLM（HTTP）互为可换后端。**坐标不丢**：几何仍来自版面/表格检测，模型只负责"读"，`resolve_citation` 页内高亮照旧成立（整页端到端模式会丢正文坐标，故不走）。env：`FASTSEARCH_VLM_URL`/`_MODEL`（必需）、`_KEY`、`FASTSEARCH_LAYOUT_MODEL`（设了才开转写）、`FASTSEARCH_VLM_MAX_PAGES`（默认 50）。顺序上 VLM 在 UniRec 之前，UniRec 跳过 `table:vlm:` 开头的表 → 两者可同配（VLM 优先 + UniRec 兜底）。+1 单测（未配 env 即恒等）；docparse 侧 `scripts/vlm_stub_e2e.py` 无 GPU 复现整条接线。见 [接入 spec](../plans/2026-07-27-OvisOCR2接入需求分析与功能设计.md)、[devlog](../devlog/2026-07-27-RegionReader接缝与VLM区域识别.md)。

- [x] **图片字节闭环（KB-1.1，2026-08-25）**：`ingest` 此前只产出图片的 `DocRegion` 坐标 + caption 文本，`media_bytes` **恒为空**——`docparse_core::chunk::ImageMeta.data_base64` 在这条路径上从没被填过（只有 docparse-cli 自己的 `--image-embed` 私有函数才填），于是"图搜图 / 引用回看显示原图 / `image_vector_status=Embedded`"全建在空地基上。现在从 `docparse_core::ir::ImageChunk.data`（`pub`，`#[serde(skip)]` 仅进程内可见）在 **CLI 侧自己搭桥**（不动 `vendor/docparse`），按 `(page, bbox)` 位模式精确连接图元素与 image chunk（`chunk_document` 原样抄坐标，无浮点运算），`std::mem::take` 取走字节不复制。新增 `--images object|inline|none`（默认 `object`）；PDF 的 `decode_images` **只在要字节时打开**。**真二进制端到端验证 2026-08-25**：起 server（`FASTSEARCH_OBJECT_DIR` + `FASTSEARCH_ASSET_SIGNING_KEY`）→ `ingest --images object` 含图 PDF/DOCX → 对象存储落盘文件 SHA-256 与源图一致 → `curl -L /v1/asset/kb:r.pdf:0` 取回 3466B `image/jpeg`，与 `figure.jpg` **逐字节相同**；`--images none` 档 `media_bytes=None`/`img_status=None`/`/v1/asset` 仍吐 `doc_render` 坐标（零回归）。+5 单测（含两份真实夹具）。收口六绿。
- [x] **分块 profile（FS-204，2026-08-31）**：`ChunkProfile` 集中校验并为 `ingest`/`index-dir` 产物写 provenance；docparse `ChunkOptions.overlap_chars` 只复用完整 layout block，引用几何不失真。真进程验证：DOCX 以 `live-docx` v4/16/4 摄取后 metadata 可反解；同一 Markdown 用 target 120/1200 分别生成 63/32 chunks。

**已知限制 / 下一迭代：**
- **`Rgb8`/`Gray8` 裸位图的字节仍不进系统**（PDF 里 Flate/CCITT/JBIG2/JPX 解出来的那类）：它们要 PNG 编码才能用，而现成编码器 `docparse_vlm::encode_png_rgb` 会把 `docparse-vlm`(+`docparse-raster`) 拉进 `parse` **轻档**，破坏"轻档无 ONNX/无渲染"；自写编码器则要新增 `png`/`flate2` 依赖。两条路都需单独评估收口，故本迭代**只落 Jpeg/Encoded 并如实标注**（`image_vector_status=missing_bytes` + stderr 计数）。
- `--images inline` 需 server 配 `DATABASE_URL`：未配 PG 时字节无处内联，`/v1/asset` 对 Inline 一律 404（真机复现 2026-08-25）。
- `ingest` 仍是**单次 POST 上传整份文档**：图片多的大 PDF 可能撞上 server 20MB `DefaultBodyLimit`（CLI 在超 16MB 时 stderr 预警并提示改 `--images none` 或走 `/v1/images`）；**分批上传**属下一迭代。
- CLI **不再离线**：所有命令需可达 server（用户决策；喂文件夹改为联网上传，反得混合检索）。
- 连接配置仅 `--server`/`--key`+env；**多 server profile**（Algolia 式）下一迭代。
- `index-dir` 已有**有界并发**（`--concurrency`）；进度 ETA / 多文件合并为单批 NDJSON（Meilisearch-importer 式）下一迭代。
- OCR/UniRec 模型需运行时下载；UniRec 自回归 CPU 慢。
- `parse-vlm` **真实模型四道门（形态/坐标/质量/速度）一道没跑**（需 vLLM+GPU，本机无 N 卡）；mock 全绿不代表质量。运行手册见接入 spec §7.5。
- `parse-vlm` 单次 VLM 调用超时 120s（`docparse-vlm` 常量）：服务**黑洞式挂起**（连得上但不回）时，每个区域都要等满这 120s。端口拒绝的场景是立即降级的，两者代价差很远。
- **VLM 自然图语义描述**（`--vlm-describe` 那条 caption 路）摄取侧仍未透传 = 下一迭代。
- `eval` 会写入目标 server 的 golden 集合——指向专用/临时集合或测试 server。
