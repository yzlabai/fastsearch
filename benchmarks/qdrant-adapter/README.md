# fastsearch client for Qdrant `vector-db-benchmark`

把 fastsearch 接入 [qdrant/vector-db-benchmark](https://github.com/qdrant/vector-db-benchmark)（Apache-2.0），与 pgvector / Qdrant / Weaviate / Elasticsearch / Milvus / OpenSearch 在**同一 harness、同一机器、同一数据集**上比较 recall-vs-QPS、p95 延迟、索引时间。

> 完整评测策略见 [docs/benchmark-strategy.md](../../docs/benchmark-strategy.md)。这里只是 client 骨架——**故意留 TODO**，跑前需补齐两处（见下）。

## 它怎么接进框架

Qdrant 的框架通过三个基类抽象一个引擎，再注册进 `ClientFactory`：

| 基类 | 职责 | 本骨架对应 |
|---|---|---|
| `BaseConfigurator` | 建集合 / 配索引参数（HNSW m/ef、量化等）→ 映射到引擎 | `FastsearchConfigurator` |
| `BaseUploader` | 批量上传向量（含 payload/过滤字段） | `FastsearchUploader` |
| `BaseSearcher` | 单条查询打分，返回 top-k id | `FastsearchSearcher` |

fastsearch 的 REST 面正好对上：上传 → `POST /v1/index`，查询 → `POST /v1/search`（`mode=vector` + 直接传 `vector`）。

## 前提

1. ✅ **`/v1/index` 已支持预计算向量**（已实现）。
   chunk 携带可选 `vector` 字段时直接走 `Engine::ingest_vector`、跳过服务端嵌入；不带则照旧嵌入 `text`。见 [server lib.rs](../../crates/fastsearch-server/src/lib.rs) 的 `IndexChunk` 与 `index` handler，回归测试 `index_with_precomputed_vector_then_vector_search`。uploader 已按此写好，开箱即用。
   ⚠️ ACL：chunk 的 `tenant`/`acl` 必须与 server API-Key 身份匹配，否则被 ACL 过滤掉（不可绕过）。benchmark 用单租户 key + chunk 带同 `tenant`/`acl` 即可。
2. ✅ **后端选择 = 服务端级 env（已收口）**。fastsearch 的向量后端（brute/brute_binary/hnsw/pgvector）是**服务端级**选择，启动时 `FASTSEARCH_VECTOR_BACKEND` 指定、落检查点持久化——**不按集合实例化**（per-collection 后端会引入「引擎侧 HashMap<collection, 后端>」的大改，违背「派生索引可重建」的简洁，且 benchmark 无此需要）。
   正确模式（与 ann-benchmarks/Qdrant 一致：**一进程一引擎配置**）：每个待比后端**起一个 server**，`configure.py` 经 `POST /v1/collections` 注册集合（dim/distance）并**读回服务端实际后端**，与实验声明不符则**显式报错**（提示 relaunch 换 env）。
3. ✅ **`ef_search` 逐查询动态可调（已实现）**——recall-vs-QPS 曲线的核心钮。`search.py` 把 `search_params["ef_search"]` 透到 `/v1/search` 请求体，**同一索引、不重启、一次查询一个值**即可扫出整条曲线（暴力/pgvector 档忽略）。实验配置里每个曲线点设不同 `ef_search` 即可。
   ⚠️ HNSW **建图期** `m`/`ef_construct` 仍取 `HnswParams::default()`，要扫这俩需扩启动 env + 各建一次索引（少数几档，暂未做，诚实记账）。

## 装配步骤

```bash
git clone https://github.com/qdrant/vector-db-benchmark
cd vector-db-benchmark
# 1) 放置 client（保持包路径 engine/clients/fastsearch/）
cp -r /path/to/fastsearch/benchmarks/qdrant-adapter engine/clients/fastsearch

# 2) 在 engine/clients/client_factory.py 注册（仿 pgvector 那几行）
#    from engine.clients.fastsearch import FastsearchConfigurator, FastsearchUploader, FastsearchSearcher
#    ENGINE_CONFIGURATORS["fastsearch"] = FastsearchConfigurator
#    ENGINE_UPLOADERS["fastsearch"]    = FastsearchUploader
#    ENGINE_SEARCHERS["fastsearch"]    = FastsearchSearcher

# 3) 配置实验（仿 experiments/configurations/pgvector-single-node.json）
#    引擎名填 "fastsearch"，写入 hnsw/quant 参数

# 4) 起 fastsearch server（嵌入后端可关，向量直传不需要嵌入）
FASTSEARCH_DATA=./data FASTSEARCH_KEYS="dev=:" fastsearch-server &

# 5) 跑
python run.py --engines fastsearch --datasets glove-100-angular
```

## 公平性（务必对齐 Qdrant 方法论）
- 相同机器（8 vCPU 级），**内存统一封顶 25 GB**。
- 单节点上传+检索 + 过滤检索两个场景。
- 指标：RPS、mean/p95 延迟、索引时间、precision（recall）。
- **不要引用 Qdrant/Zilliz 的现成胜负数字**——全部自机重跑。
