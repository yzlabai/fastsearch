# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> 开发流程/规范见 [AI_AGENT_DEV_SPEC.md](AI_AGENT_DEV_SPEC.md)（开工前先读，尤其"两条路径"与收口约束）。
> 背景与决策见 [需求分析报告](docs/plans/2026-06-24-需求分析报告.md) 与 [产品设计文档](docs/plans/2026-06-24-产品设计文档.md)；模块清单见 [docs/specs/00-模块拆分.md](docs/specs/00-模块拆分.md)。**代码现状永远是真源**：本文件/规范与代码不符时以代码为准并回写。

## 命令

```bash
cargo build                                    # 构建（release: cargo build --release，lto=thin）
cargo test --workspace                         # 全部单测
cargo test -p fastsearch-core                  # 单 crate
cargo test -p fastsearch-core fusion           # 按名/模块过滤跑部分测试
cargo clippy --workspace --all-targets -- -D warnings   # lint，目标零 warning
cargo fmt --all            # 格式化（--check 仅校验）

# 起 server（真源/索引/嵌入/落盘都在这）
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:public" cargo run -p fastsearch-server --bin fastsearch-server  # REST :8642

# CLI 是 server 的**纯 REST 客户端**（--server/--key，或 env FASTSEARCH_SERVER/FASTSEARCH_KEY）
cargo run -p fastsearch-cli --bin fastsearch -- index --server http://localhost:8642 --key dev --collection kb --doc-id r.pdf chunks.json
cargo run -p fastsearch-cli --bin fastsearch -- search --server http://localhost:8642 --key dev --collection kb --query "毛利率" --json
cargo run -p fastsearch-cli --bin fastsearch -- index-dir --server http://localhost:8642 --key dev --collection kb ./docs   # 喂文件夹（客户端分块→上传）

# 多格式解析摄取（docparse 融合，--features parse；解析在客户端→POST /v1/index）：PDF/DOCX/HTML/MD/CSV/XLSX/PPTX/SRT/EML + 图片
cargo run -p fastsearch-cli --features parse --bin fastsearch -- ingest --server http://localhost:8642 --key dev --collection kb --doc-id r.docx r.docx
# 扫描件/图片 OCR 抽文本（--features parse-ocr，需 PP-OCR ONNX 模型目录）
FASTSEARCH_OCR_MODELS=/path/to/docparse-rs/models/ppocr-v5 \
  cargo run -p fastsearch-cli --features parse-ocr --bin fastsearch -- ingest --server http://localhost:8642 --key dev --collection kb --doc-id scan.png scan.png
# 表格结构识别（非 VLM 的确定性 ONNX：UniRec；--features parse-tables，需 UniRec 模型目录）
FASTSEARCH_UNIREC_MODELS=/path/to/docparse-rs/models/unirec \
  cargo run -p fastsearch-cli --features parse-tables --bin fastsearch -- ingest --server http://localhost:8642 --key dev --collection kb --doc-id r.pdf r.pdf
# VLM 区域识别（--features parse-vlm，需外部 OpenAI 兼容服务；表格→HTML，另设版面模型则加区域级转写）
FASTSEARCH_VLM_URL=http://localhost:8000 FASTSEARCH_VLM_MODEL=OvisOCR2 \
  cargo run -p fastsearch-cli --features parse-vlm --bin fastsearch -- ingest --server http://localhost:8642 --key dev --collection kb --doc-id r.pdf r.pdf

# Postgres 集成测试（默认跳过；设 DATABASE_URL 才跑，CI 用 pgvector/pgvector 镜像）
DATABASE_URL=postgres://user@localhost/db cargo test -p fastsearch-pg
# 独立摄取 worker（与 server 共用 PG/ObjectStore；key 还须列入 server 的 FASTSEARCH_WORKER_KEYS）
DATABASE_URL=postgres://user@localhost/db FASTSEARCH_SERVER=http://localhost:8642 \
FASTSEARCH_WORKER_KEY=worker FASTSEARCH_OBJECT_DIR=./objects \
  cargo run -p fastsearch-ingest-worker --bin fastsearch-ingest-worker
# OCR 端到端测试（默认跳过；设模型目录 + 测试图才跑）
FASTSEARCH_OCR_MODELS=…/models/ppocr-v5 FASTSEARCH_OCR_TEST_IMAGE=…/page.png cargo test -p fastsearch-cli --features parse-ocr ocr_end_to_end
```

> **构建分档**：默认 `cargo build`（搜索热路径，**零 docparse/ONNX 依赖**）；`--features parse`（多格式解析，轻量、无 ONNX）；`--features parse-ocr`（+PP-OCR 扫描件抽文本）；`--features parse-tables`（+UniRec **非 VLM** 表格结构识别，拉 raster/tract ONNX）；`--features parse-vlm`（+**VLM 区域识别**，需外部 OpenAI 兼容服务，env 指 URL+模型名）。重档运行时需指模型目录/服务地址（env），仅摄取侧；`vendor/docparse` 有自有 workspace、被根 `exclude`，不进默认收口。
>
> **识别后端可换**（2026-07-27）：`docparse-core::region_reader::RegionReader` 是"区域图→文本"的**唯一接缝**——`UniRec`（进程内 tract）与 `VlmRegionReader`（HTTP 服务）互为实现，表格重识别/整页转写两处编排对二者一视同仁。并发策略归后端（`read_batch` 默认串行，HTTP 后端覆写为有界并发）。**坐标始终来自版面/表格检测**，VLM 只负责"读"，故 `resolve_citation` 页内高亮不受影响。

**收口（push 前必跑，等价于"完整类型检查"）**：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 三者全过，详见 AI_AGENT_DEV_SPEC §收口。

## 架构大图（需要跨文件才能看懂的部分）

**一句话**：外部单二进制混合检索引擎，**以托管 Postgres(pgvector) 为真源**；引擎侧索引是**派生、可重建**的。差异化（超越 ParadeDB）= **只需 pgvector + 逻辑复制、不在 PG 装任何原生扩展**，因此能跑在任意托管 PG（RDS/Supabase/Neon）。

> **docparse 已 subtree 并入本仓 `vendor/docparse/`**（融合 Option B，2026-06-27）：它保留**自有 workspace**（含 vendored tract + OCR/VLM/raster 重 ONNX），经根 `exclude` 与 fastsearch 精简构建隔离；共享 `fastsearch-ingest-adapter` 的 **`parse` feature** 才 path-依赖 docparse，CLI 与独立 worker 共用这一份实现。默认 CLI 只依赖 adapter 的轻量 profile 类型。**搜索热路径零 docparse 依赖**（`cargo tree` 校验）。见 [融合方案评估](docs/plans/2026-06-26-docparse融合方案评估.md)。

```
原始文档 → POST /v1/documents → ObjectStore + Postgres ingest_job（状态/身份真源）
                                      │ fastsearch-ingest-worker 领取、解析、heartbeat、job-scoped 写入
                                      ▼
docparse chunks（vendor/docparse 解析 / 或外部 chunks.json）→ Postgres(真源: chunks 表 + 元数据 + ACL + pgvector 向量列)
                       │ 逻辑复制 CDC（pgoutput, 幂等, LSN 续传）   ← fastsearch-sync
                       ▼
   fastsearch 引擎（无状态, 可多副本）  ← fastsearch-engine 编排
     派生 BM25 倒排(Tantivy/mmap) + 向量 ANN(filter-aware) + 元数据/ACL
   排序管线：ACL 强制注入 → keyword∥vector 召回 → 融合 → rerank → 高亮/分面 → top-K
   四张脸：REST(server，嵌引擎) · 库 · MCP(stdio+JSON-RPC) · CLI(REST 客户端) — search/resolve_citation，ACL 服务端注入
答案层(外部 LLM)：resolve_citation(id) → {page,bbox} → 深链/高亮
```

**crate 依赖分层（自底向上，上不依赖下）**——`fastsearch-core` 不依赖任何后端，所有后端经 trait 边界接入：

| crate | 角色 | 关键点 |
|---|---|---|
| `core` | 纯逻辑：文档模型 / 查询·过滤 AST / 融合(RRF·归一化·加权) / 引用 / **ACL** | 无后端依赖；`Filter::eval`、`AclFilter::visible`、`fuse` 是确定性的（同分按 `GlobalId` tie-break） |
| `text` | Tantivy BM25 + 分词(jieba/default) + 过滤 + 高亮 | 见下"预过滤策略"；`text` 字段 STORED 供高亮/rerank |
| `vector` | 向量后端 **trait** + `MemVectorIndex`(暴力余弦) + HNSW + 二值/RaBitQ 量化 | **filter-aware 真预过滤**（打分前过滤，超 pgvector 后过滤坑）；HNSW 图内 filtered-traversal + 墓碑压实 + 暴力精确安全网(H5) 已落地，见 [spec 15](docs/specs/15-vector.md) |
| `embed` / `rerank` | `Embedder` / `Reranker` trait + 确定性基线 + 可配置 HTTP 嵌入后端 | HashEmbedder/LexicalOverlap 非语义，仅离线/CI/fallback；**真语义嵌入经 `HttpEmbedder`（Ollama / OpenAI 兼容）接入，不做进程内模型推理**；rerank：RAG 主路径默认不上神经 rerank（答案层 LLM 兜底），可选 LTR 供无-LLM 入口 |
| `pg` | Postgres 真源：DDL + Chunk↔行映射 + doc 级替换写路径 | DDL 在 `sql.rs`，**只用 pgvector + 逻辑复制**；集成测试 env-gated |
| `sync` | CDC apply 编排：幂等 + LSN 水位续传 + 替换语义 | `IndexSink` trait；pgoutput 解码器已落地（UnchangedToast→真源重取 H3、批量上限 M16）；仅 `START_REPLICATION` 流式为后续（现走轮询式 `pg_logical_slot_get`，正确、崩溃安全） |
| `engine` | 整合 text+vector+rerank+sync sink → 端到端排序管线 | `run()` 是管线主体；`search`/`search_with_facets`；实现 `sync::IndexSink`（适配器，避免 text 反依赖 sync） |
| `eval` | 相关性评测：nDCG/recall/MRR + `assert_no_regression`(CI 门禁) | 纯函数 |
| `server` | REST(axum) + API-Key 认证 + **ACL 服务端注入不可绕过** + 上传/job 查询与 worker 写协议 + /metrics | `POST /v1/documents` 只保存原始对象并建立 job；worker 写入时 tenant/ACL/文档坐标只从 job 恢复；检索仍由 `principal_from_headers`→`acl_for` 强制过滤 |
| `ingest-adapter` | CLI/worker 共用的 docparse→core chunk 适配与 `ChunkProfile` | 默认构建只有轻量类型；`parse*` feature 才引入 docparse/OCR/VLM |
| `ingest-worker` | 独立 claim/heartbeat/ObjectStore fetch/parse/job-scoped publish 进程 | 每并发槽独立 JobStore；wire DTO 不含 collection/doc_id/tenant/acl；不直接写 chunks 真源 |
| `cli` | `fastsearch` 二进制：**server 的纯 REST 客户端**；`ingest` 调共享 adapter 后 POST `/v1/index` | 默认无 docparse；`parse*` feature 转发给 adapter；检索/嵌入/落盘全归 server |
| `mcp` | 第四张脸：MCP 暴露检索/引用；远端档还有 chunk 写入与能力门控的文档摄取/状态工具 | `document_ingest` 只从 live server caps 推导；**ACL 服务端注入不可绕过** |
| `clients/{python,ts}` | 零依赖 SDK（封装 REST）+ LangChain/LlamaIndex 适配 | — |

## 关键不变量（跨 crate 都要守）

1. **托管 PG 可移植（硬约束）**：PG 侧只能用 **pgvector + 逻辑复制**，**绝不要求 `shared_preload_libraries` 原生扩展**。这是超越 ParadeDB 的根；CI/评审守住（见 N1b、§6.8）。
2. **PG 是真源，引擎索引是派生**：崩溃恢复=重放复制流 / 从快照重建。别在引擎侧引入"只在引擎有、PG 没有"的权威数据。
3. **ACL 不可绕过**：ACL 只来自认证身份，服务端在过滤期强制注入（`engine.search` 的 `acl` 参数），客户端不能在请求体里传或放宽。新增检索入口必须走这条。
4. **确定性**：融合/检索同分按 `GlobalId` 升序 tie-break；同输入+同稳定索引→同结果。
5. **预过滤策略（text/vector）**：把过滤翻译成 **SUPERSET** 后端查询（不可翻译子句→match-all，保召回）+ 用 `core::Filter::eval`/`AclFilter::visible` 做**精确后过滤**（保精度/不越权）；over-fetch 抵消截断。改过滤逻辑两端都要守。
6. **重依赖 opt-in、诚实记账**：HNSW/量化、流式 CDC、神经 rerank 等未落地的，文档与状态如实标 `下一迭代`/`待运行验证`，别写"已完成"（见 AI_AGENT_DEV_SPEC）。真语义嵌入经外部 HTTP 服务（Ollama/OpenAI 兼容），不引进程内模型推理依赖。

## 数据模型锚点

- `core::Chunk` 字段与 docparse chunk schema 对齐（kind/page/bbox/heading_path/section_id/char_len），外加 `tenant`/`acl`。
- `GlobalId = (collection, doc_id, chunk_id)`；`citation_id` = `"{collection}:{doc_id}:{chunk_id}"`（doc_id 可含 `:`，反解取首段/末段）。因为身份中不含 tenant，`(collection, doc_id)` 是系统全局文档坐标且只能由一个 tenant 持有；客户端需自行命名空间化，见 [ADR-0001](docs/adr/0001-文档坐标采用全局命名空间.md)。
- PG 表结构、INSERT/DELETE、行↔Chunk 映射全在 [`crates/fastsearch-pg/src/sql.rs`](crates/fastsearch-pg/src/sql.rs)（纯函数，可单测）；改 schema 改这里并更新 DDL 测试。
