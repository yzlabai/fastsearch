# fastsearch

**外部单二进制混合检索引擎，以托管 Postgres(pgvector) 为真源。** 专为 docparse-rs 解析后的可溯源文档 chunk 做全文 / 向量 / 混合检索，把 page+bbox 引用端到端带到答案层。

> 超越 ParadeDB 的关键：**能跑在任意托管 Postgres（RDS/Supabase/Neon）上**——ParadeDB/VectorChord/pg_textsearch 因需 `shared_preload_libraries` 原生扩展都做不到。fastsearch 只需 pgvector + 逻辑复制。

## 设计文档

- [需求分析报告](docs/plans/2026-06-24-需求分析报告.md)（v3，含 §4.5 产品完备性 12 维、§6.8 架构决议、竞品全景）
- [产品设计文档](docs/plans/2026-06-24-产品设计文档.md)（技术架构 / 功能设计 / API / 开发计划）
- [模块拆分与 spec 索引](docs/specs/00-模块拆分.md)

## 架构（Quickwit 式控制面/数据面分离）

```
docparse chunks → Postgres(真源: chunk+元数据+ACL+pgvector)
                       │ 逻辑复制 CDC（pgoutput, 幂等, LSN 续传）
                       ▼
   fastsearch 引擎（单二进制, 无状态多副本）
     · 派生 BM25 倒排（Tantivy/mmap）   · 向量 ANN（filter-aware）
     · 一等融合(RRF/归一化/加权)         · 逐文档 ACL 强制注入
     · 引用溯源(page+bbox+section)
   四张脸：CLI · 库 · REST · MCP
```

## 已实现模块（workspace crates）

| crate | 职责 | 测试 | 状态 |
|---|---|---|---|
| `fastsearch-core` | 文档模型、查询/过滤 AST、融合(RRF/归一化/加权)、引用、ACL | 18 | ✅ v1 |
| `fastsearch-text` | Tantivy BM25 + CJK(jieba) + 过滤 + ACL + upsert/delete | 10 | ✅ v1 |
| `fastsearch-pg` | Postgres 真源：DDL、Chunk↔行映射、doc 级替换写路径 | 6 | ✅ v1（集成 env-gated）|
| `fastsearch-sync` | CDC apply 编排：幂等、LSN 水位、批量、替换语义 | 5 | ✅ v1（线缆层待续）|
| `fastsearch-vector` | filter-aware 向量后端（真预过滤，超 pgvector 后过滤坑） | 7 | ✅ v1（HNSW/量化待续）|
| `fastsearch-embed` | 嵌入后端 trait + 确定性离线基线 | 5 | ✅ v1（Candle/ONNX 待续）|
| `fastsearch-engine` | 整合：ingest→CDC→索引→**全文/向量/真混合**检索→引用 | 9 | ✅ v1 |
| `fastsearch-cli` | 可运行 `fastsearch` 二进制：吃 docparse chunks 建库 + 检索 | 5 | ✅ v1 |

**端到端可用**：ingest → CDC 同步 → 索引 → 三模式检索（keyword/vector/hybrid）→ 带引用命中，ACL 强制不可绕过。

## 快速试用（CLI）

```bash
cargo build -p fastsearch-cli --bin fastsearch
# 灌入 docparse chunks（JSON 数组或 NDJSON），落盘到 ./data
docparse report.pdf -f chunks | ./target/debug/fastsearch index --data ./data --collection kb --doc-id report.pdf
# 检索（带 page+bbox+heading_path 引用；支持 --kind/--page-min/--page-max 过滤、--json）
./target/debug/fastsearch search --data ./data --collection kb --query "毛利率" --json
```

## 构建与测试

```bash
cargo test --workspace        # 65 测试全绿（PG 集成测试在有 DATABASE_URL 时运行）
cargo clippy --workspace --all-targets   # 零 warning
cargo fmt --all --check
```

PG 集成测试：`DATABASE_URL=postgres://... cargo test -p fastsearch-pg`（CI 用 `pgvector/pgvector` 镜像）。

## 路线图（通往 1.0 GA，见设计文档 §8）

- **P1（进行中）**：PG 真源 + 健壮 CDC 线缆层、全文、评测护栏。
- **P2**：真语义嵌入（Candle e5）、HNSW+量化、pgvector 直查档。
- **P3**：auto-merging、分面/高亮、k1/b 调优、相关性评测体系。
- **P4**：server（REST/MCP + 认证/逐文档 ACL + 可观测 + 零停机生命周期）。
- **P5**：CLI + Python/TS SDK + 基准 vs ParadeDB → GA。

## 许可

Apache-2.0。注意 lindera 中文字典（CC-CEDICT, CC-BY-SA）若启用需审分发条款。
