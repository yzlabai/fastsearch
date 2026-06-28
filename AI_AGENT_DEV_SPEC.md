# AI Agent 开发规范 · fastsearch

> 给 coding agent 的开发流程与约束。项目特有约定在 [CLAUDE.md](CLAUDE.md)；架构/决策在
> [需求分析报告](docs/plans/2026-06-24-需求分析报告.md) 与 [产品设计文档](docs/plans/2026-06-24-产品设计文档.md)。
> **代码现状永远是真源**：本文与代码冲突时，以代码为准并回写本文。

## 0. 开发流程（开工前先判复杂度，再选路径）

**第一步永远是判断需求复杂度**，再选下面两条路径之一。**简单需求别套全套**；拿不准就按复杂处理（先写计划）。

### 复杂需求
（新 crate / 新后端 / 跨 crate 改动 / 影响检索结果或数据契约 / 触及关键不变量 / 有不确定性 → 需要先想清楚）

1. 写**计划/spec 文档** `docs/specs/<n>-<module>.md`（新模块）或 `docs/plans/YYYY-MM-DD-<topic>.md`（跨模块/特性）——含**需求三件套**（要做什么/为什么/验收）、范围与"不做什么"、**用户使用例子**、**测试用例**、验收标准。spec 标准结构见 [docs/specs/00-模块拆分.md §4](docs/specs/00-模块拆分.md)。
2. **review 相关文档**（spec/plan + 关联的产品设计章节 + 受影响模块的现有 spec），确认方向、落点、不变量无误，再动手。
3. **实施**（源码 + 测试代码同一改动；偏离计划就回写 spec/plan）。
4. **测试**——`cargo test --workspace` 单测 + `cargo clippy --workspace --all-targets -- -D warnings`；涉及 Postgres 的设 `DATABASE_URL` 跑集成；有问题就**修复并 review**，循环直到无遗留。
5. **更新文档**——回写对应 spec 的"状态/已知限制/下一迭代"，必要时更新 [README](README.md)/CLAUDE.md。
6. **review** 全部改动与文档 → `commit` → `push`。

### 简单需求
（单 crate 局部改、bugfix、加 CLI 选项、小重构 → 落点清晰、可逆）

1. 先**实施**。
2. **补充/更新文档**（对应 spec 的迭代记录、必要注释）。
3. **测试**——单测 + clippy；涉及检索结果/解析/契约时跑相关 crate + 端到端（CLI/server 实跑）。
4. **review** → `commit` → `push`。

## 1. 本项目特有约束（两条路径都适用）

- **守住"托管 PG 可移植"**（最高优先级不变量）：PG 侧只能依赖 **pgvector + 逻辑复制**，**绝不要求 `shared_preload_libraries` 原生扩展**。任何引入 PG 侧扩展依赖的改动都破坏了超越 ParadeDB 的根，需在 plan 里专门论证并评审。DDL 改动在 [`crates/fastsearch-pg/src/sql.rs`](crates/fastsearch-pg/src/sql.rs)，须保持只用 `CREATE EXTENSION vector` + `CREATE PUBLICATION`。
- **PG 是真源，引擎索引是派生**：写路径落 PG；引擎侧 Tantivy/向量索引可从 PG 重建。别在引擎侧造"PG 没有"的权威数据；崩溃恢复靠重放复制流/快照，不靠引擎自建 WAL。
- **ACL 不可绕过**：新增任何检索入口（REST 路由 / MCP 工具 / 库 API），ACL 必须由认证身份在服务端经 `engine.search(req, Some(&acl))` 注入，客户端不能传/放宽。新增入口要有"越权用例进测试"（见 server 的 `acl_not_bypassable`）。
- **代码完成 ≠ 完成**：未经**运行/真机验证**前，文档与提交里如实标 **`待运行验证`**，别写"已完成"。本仓惯例：纯逻辑用单测验证；PG 路径用 `DATABASE_URL` 集成（无则 env-gated 跳过）；CLI/server 用**实跑二进制 + curl/SDK** 验证（见各 spec"活服务验证"）。验证通过再改状态。
- **重依赖 opt-in、诚实记账**：未落地的重件——**流式 pgoutput 线缆层**、**进程内跨模态/ColPali 模型**、**对象存储签名 URL/Range**——文档里如实标 `下一迭代`/`gated`，当前用确定性基线占位（HashEmbedder / MemVectorIndex 暴力余弦 / LexicalOverlapReranker / sync apply 核心 + pgoutput 解码 + SQL 轮询消费）。**别把基线说成语义/生产级**。（**已落地、不再属"未落地"**：HNSW+u8 量化 + 二值量化粗筛、k1/b 自定义 BM25、MCP 第四张脸、B6 CDC 写穿、docparse 多格式/OCR/表格摄取——见各 spec 与看板。）真语义嵌入经**可配置 HTTP 后端**（Ollama / OpenAI 兼容，见 `fastsearch-embed::HttpEmbedder`）接入，**不引进程内模型推理（Candle/ort）依赖**。
- **预过滤两端一致**：改 `core::Filter` 或过滤翻译时，`text`（Tantivy query 翻译）与 `vector`（filter-aware 召回）+ 各自的精确后过滤都要同步守住"SUPERSET 预过滤 + 精确后过滤"语义（CLAUDE.md 不变量 §5）。
- **确定性**：新增排序/融合/检索路径，并列项一律按 `GlobalId` 升序 tie-break，保证可复现、便于 golden 回归。
- **依赖集中管理**：版本写在根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 用 `dep.workspace = true` 继承；新依赖先问/在 plan 写清，优先纯 Rust、MIT/Apache（注意 lindera 中文字典 CC-BY-SA，分发前审）。

## 2. 收口（两条路径都不可省）

最终都要**更新对应 spec/功能说明 + review + 测试通过 + 完整 lint 通过**，才 `commit`、`push`：

```bash
cargo fmt --all --check                              # 格式
cargo clippy --workspace --all-targets -- -D warnings   # 零 warning（等价"完整类型检查"，不能省）
cargo test --workspace                               # 全绿
# 改了检索行为/契约：再用 CLI 或 server 二进制实跑验证一遍
```

> 注意：`cargo build` 通过 ≠ 收口完成。**clippy `-D warnings` 是硬门禁**（本仓目标零 warning）；只 build 不 clippy 会漏掉可疑/正确性 lint。涉及 Postgres 的改动，能起 PG 就跑集成测试，不能起就在 PR/状态里标 `待运行验证`。

## 3. commit / push 约定

- 在默认分支（main）上：用户**显式要求**才 push；否则先开分支。本仓已有远端 `git@github.com:yzlabai/fastsearch.git`。
- commit message 收尾加：
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
- DDL/数据契约改动：同一 commit 内更新 `sql.rs` 的 DDL 与对应 golden/单测，别让 schema 与测试漂移。

## 4. 文档落点

- `docs/plans/`：需求分析、产品设计、跨模块开发计划（复杂需求开工前写）。
- `docs/specs/<n>-<module>.md`：每模块一份 spec（目的/接口/行为/测试/验收/状态/迭代记录）；完成一轮迭代回写"状态 + 已知限制/下一迭代"。
- `README.md` / `CLAUDE.md`：对外简介 / 给 agent 的速查与不变量——有结构性变化才动。
- 不重复：能从代码/`cargo`/spec 发现的细节不抄进 CLAUDE.md。
