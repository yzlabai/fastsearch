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

# 跑二进制（四张脸里的 CLI / REST）
cargo run -p fastsearch-cli --bin fastsearch -- index --data ./data --collection kb --doc-id r.pdf chunks.json
cargo run -p fastsearch-cli --bin fastsearch -- search --data ./data --collection kb --query "毛利率" --json
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" cargo run -p fastsearch-server --bin fastsearch-server  # REST :8642

# Postgres 集成测试（默认跳过；设 DATABASE_URL 才跑，CI 用 pgvector/pgvector 镜像）
DATABASE_URL=postgres://user@localhost/db cargo test -p fastsearch-pg
```

**收口（push 前必跑，等价于"完整类型检查"）**：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 三者全过，详见 AI_AGENT_DEV_SPEC §收口。

## 架构大图（需要跨文件才能看懂的部分）

**一句话**：外部单二进制混合检索引擎，**以托管 Postgres(pgvector) 为真源**；引擎侧索引是**派生、可重建**的。差异化（超越 ParadeDB）= **只需 pgvector + 逻辑复制、不在 PG 装任何原生扩展**，因此能跑在任意托管 PG（RDS/Supabase/Neon）。

```
docparse chunks → Postgres(真源: chunks 表 + 元数据 + ACL + pgvector 向量列)
                       │ 逻辑复制 CDC（pgoutput, 幂等, LSN 续传）   ← fastsearch-sync
                       ▼
   fastsearch 引擎（无状态, 可多副本）  ← fastsearch-engine 编排
     派生 BM25 倒排(Tantivy/mmap) + 向量 ANN(filter-aware) + 元数据/ACL
   排序管线：ACL 强制注入 → keyword∥vector 召回 → 融合 → rerank → 高亮/分面 → top-K
   四张脸：CLI · 库 · REST · MCP(待)
答案层(外部 LLM)：resolve_citation(id) → {page,bbox} → 深链/高亮
```

**crate 依赖分层（自底向上，上不依赖下）**——`fastsearch-core` 不依赖任何后端，所有后端经 trait 边界接入：

| crate | 角色 | 关键点 |
|---|---|---|
| `core` | 纯逻辑：文档模型 / 查询·过滤 AST / 融合(RRF·归一化·加权) / 引用 / **ACL** | 无后端依赖；`Filter::eval`、`AclFilter::visible`、`fuse` 是确定性的（同分按 `GlobalId` tie-break） |
| `text` | Tantivy BM25 + 分词(jieba/default) + 过滤 + 高亮 | 见下"预过滤策略"；`text` 字段 STORED 供高亮/rerank |
| `vector` | 向量后端 **trait** + `MemVectorIndex`(暴力余弦) | **filter-aware 真预过滤**（打分前过滤，超 pgvector 后过滤坑）；HNSW/量化待迭代 |
| `embed` / `rerank` | `Embedder` / `Reranker` trait + 确定性基线 + 可配置 HTTP 嵌入后端 | HashEmbedder/LexicalOverlap 非语义，仅离线/CI/fallback；**真语义嵌入经 `HttpEmbedder`（Ollama / OpenAI 兼容）接入，不做进程内模型推理**；rerank：RAG 主路径默认不上神经 rerank（答案层 LLM 兜底），可选 LTR 供无-LLM 入口 |
| `pg` | Postgres 真源：DDL + Chunk↔行映射 + doc 级替换写路径 | DDL 在 `sql.rs`，**只用 pgvector + 逻辑复制**；集成测试 env-gated |
| `sync` | CDC apply 编排：幂等 + LSN 水位续传 + 替换语义 | `IndexSink` trait；pgoutput 线缆层待迭代（v1 只做正确性核心） |
| `engine` | 整合 text+vector+rerank+sync sink → 端到端排序管线 | `run()` 是管线主体；`search`/`search_with_facets`；实现 `sync::IndexSink`（适配器，避免 text 反依赖 sync） |
| `eval` | 相关性评测：nDCG/recall/MRR + `assert_no_regression`(CI 门禁) | 纯函数 |
| `server` | REST(axum) + API-Key 认证 + **ACL 服务端注入不可绕过** + /metrics | `principal_from_headers`→`acl_for`→`engine.search(req, Some(acl))`；客户端无法传/绕过 ACL |
| `cli` | `fastsearch` 二进制：吃 docparse chunks → 落盘 text 索引 → 检索 | 逻辑在 lib，main 是壳 |
| `clients/{python,ts}` | 零依赖 SDK（封装 REST） | — |

## 关键不变量（跨 crate 都要守）

1. **托管 PG 可移植（硬约束）**：PG 侧只能用 **pgvector + 逻辑复制**，**绝不要求 `shared_preload_libraries` 原生扩展**。这是超越 ParadeDB 的根；CI/评审守住（见 N1b、§6.8）。
2. **PG 是真源，引擎索引是派生**：崩溃恢复=重放复制流 / 从快照重建。别在引擎侧引入"只在引擎有、PG 没有"的权威数据。
3. **ACL 不可绕过**：ACL 只来自认证身份，服务端在过滤期强制注入（`engine.search` 的 `acl` 参数），客户端不能在请求体里传或放宽。新增检索入口必须走这条。
4. **确定性**：融合/检索同分按 `GlobalId` 升序 tie-break；同输入+同稳定索引→同结果。
5. **预过滤策略（text/vector）**：把过滤翻译成 **SUPERSET** 后端查询（不可翻译子句→match-all，保召回）+ 用 `core::Filter::eval`/`AclFilter::visible` 做**精确后过滤**（保精度/不越权）；over-fetch 抵消截断。改过滤逻辑两端都要守。
6. **重依赖 opt-in、诚实记账**：HNSW/量化、流式 CDC、神经 rerank 等未落地的，文档与状态如实标 `下一迭代`/`待运行验证`，别写"已完成"（见 AI_AGENT_DEV_SPEC）。真语义嵌入经外部 HTTP 服务（Ollama/OpenAI 兼容），不引进程内模型推理依赖。

## 数据模型锚点

- `core::Chunk` 字段与 docparse chunk schema 对齐（kind/page/bbox/heading_path/section_id/char_len），外加 `tenant`/`acl`。
- `GlobalId = (collection, doc_id, chunk_id)`；`citation_id` = `"{collection}:{doc_id}:{chunk_id}"`（doc_id 可含 `:`，反解取首段/末段）。
- PG 表结构、INSERT/DELETE、行↔Chunk 映射全在 [`crates/fastsearch-pg/src/sql.rs`](crates/fastsearch-pg/src/sql.rs)（纯函数，可单测）；改 schema 改这里并更新 DDL 测试。
