# fastsearch

**外部单二进制混合检索引擎，以托管 Postgres(pgvector) 为真源。** 为可溯源文档 chunk（docparse-rs 解析或纯文本/markdown）做全文 / 向量 / 混合检索，把 **page+bbox 引用端到端**带到答案层——专为 **AI Agent / RAG 的检索与 grounding** 而生。

> 👉 **用于 Agent 开发？先读 [在 Agent 中使用 fastsearch](docs/在Agent中使用fastsearch.md)**（四张脸接入 · RAG recipe · MCP · 多租户 ACL · 同类对比）。

> 超越 ParadeDB 的关键：**能跑在任意托管 Postgres（RDS/Supabase/Neon）上**——只需 pgvector + 逻辑复制，**不要求任何 `shared_preload_libraries` 原生扩展**。

## 架构

```
docparse chunks / 文本文件 → Postgres(真源: chunk+元数据+ACL+pgvector)
                       │ 逻辑复制 CDC（pgoutput, 幂等, LSN 续传）
                       ▼
   fastsearch 引擎（单二进制, 无状态多副本, 派生索引可重建）
     · BM25 倒排(Tantivy/mmap)   · 向量(暴力/HNSW+u8量化/pgvector直查, filter-aware)
     · 融合(RRF/归一化/加权)      · 逐文档 ACL 强制注入(不可绕过)
     · 引用溯源(page+bbox+section) + resolve_citation 深链 · 多模态
   四张脸：CLI · 库 · REST · MCP
```

## 五分钟上手（本机零依赖）

```bash
cargo build -p fastsearch-cli --bin fastsearch
# 喂一个资料文件夹（递归 .md/.txt，markdown 标题成面包屑），随后检索
./target/debug/fastsearch index-dir --data ./idx --collection kb  ./我的资料
./target/debug/fastsearch search    --data ./idx --collection kb --query "毛利率" --json
```

接 docparse / PDF / REST / MCP / Python 的用法见 [Agent 使用指南](docs/在Agent中使用fastsearch.md)。

## 模块（workspace crates）

| crate | 职责 |
|---|---|
| `fastsearch-core` | 文档模型、查询/过滤 AST、融合(RRF/归一化/加权)、引用、**ACL** |
| `fastsearch-text` | Tantivy BM25 + CJK(jieba) + 过滤 + 高亮/分面 + ACL |
| `fastsearch-vector` | 向量后端三档：暴力(默认确定) / HNSW+u8量化(近似) / pgvector直查；filter-aware |
| `fastsearch-embed` | 嵌入后端 trait + 可配置 HTTP 后端（Ollama / OpenAI 兼容） |
| `fastsearch-pg` | Postgres 真源：DDL、Chunk↔行映射、doc 级替换写、pgvector 直查 |
| `fastsearch-sync` | CDC apply：pgoutput 解码 + 幂等 + LSN 检查点 + 替换语义 |
| `fastsearch-engine` | 整合：ingest→CDC→索引→**全文/向量/混合**检索→引用 + 深分页 + 重建 + 媒资解析 |
| `fastsearch-eval` | 相关性评测：golden 集 + nDCG/recall/MRR + CI 回归门禁 |
| `fastsearch-server` | REST(axum) + API-Key 认证 + **ACL 不可绕过** + 指标/限流/审计 + 媒资网关 + CDC 生命周期 |
| `fastsearch-mcp` | 第四张脸：MCP(stdio+JSON-RPC) 暴露 `search`/`resolve_citation` 工具 |
| `fastsearch-cli` | `fastsearch` 二进制：index / index-dir / search / ingest(PDF) / eval |
| `clients/{python,ts}` | 零依赖 SDK + LangChain/LlamaIndex 适配 |

**端到端可用**：ingest/CDC → 索引 → 三模式检索（keyword/vector/hybrid）→ 带引用命中，ACL 强制不可绕过。四张脸齐全。

## 构建与测试

```bash
cargo test --workspace                                    # 全绿（PG 集成在有 DATABASE_URL 时运行）
cargo clippy --workspace --all-targets -- -D warnings     # 零 warning
cargo fmt --all --check
DATABASE_URL=postgres://... cargo test -p fastsearch-pg   # PG 集成（CI 用 pgvector/pgvector 镜像 + wal_level=logical）
```

## 文档

- **[在 Agent 中使用 fastsearch](docs/在Agent中使用fastsearch.md)**（开发者使用指南）
- [架构速查 / 命令 / 不变量（CLAUDE.md）](CLAUDE.md)
- [模块拆分与 spec 索引](docs/specs/00-模块拆分.md)
- [需求分析](docs/plans/2026-06-24-需求分析报告.md) · [产品设计](docs/plans/2026-06-24-产品设计文档.md)
- [部署](deploy/) · [容量与 SLO](docs/governance/2026-06-26-容量与SLO.md)

## 许可

Apache-2.0。分词词典用 jieba-rs（**MIT**，含内嵌 dict）——无 CC-BY-SA 等 share-alike 义务，分发附 MIT 归属即可（见 [许可审](docs/governance/2026-06-26-词典与第三方许可审.md)）。
