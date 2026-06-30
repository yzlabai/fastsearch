# fastsearch Benchmark 策略

> 目标：给 fastsearch（单二进制混合检索引擎，托管 Postgres+pgvector 为真源；BM25 用 Tantivy，向量支持暴力/HNSW+u8量化/pgvector直连，融合 RRF/归一化/加权，带页码+bbox 引用溯源）设计一套**可信、可复现、可对标主流竞品**的评测方法。
>
> 调研日期 2026-06-29，来源经多源对抗验证（详见末尾「来源」）。**结论先行：没有任何现成 harness 能端到端测 fastsearch 的完整混合管线，质量与性能要用两条互补工具链分别评。**

## 0. 一图：评测分两条线

| 维度 | 谁是事实标准 | 主指标 | fastsearch 如何接入 |
|---|---|---|---|
| **检索质量/相关性** | **BEIR** + Anserini/**Pyserini** 复现矩阵 | nDCG@10、Recall@100 | 复用数据集/qrels/指标，引用 Pyserini 公开基线，自跑 fastsearch |
| **向量性能** | **ann-benchmarks**（MIT） | recall-vs-QPS 前沿 | 跑同款 HDF5 数据集（需 REST 适配器） |
| **性能 + 成本一体** | Qdrant `vector-db-benchmark`（Apache-2.0）、Zilliz `VectorDBBench`（MIT，带 QP$） | RPS/p95/p99、索引时间、内存、QP$ | 写 client 适配器（Qdrant 框架接入摩擦最低） |

**关键现实**：质量 harness 不测 QPS/延迟/内存/成本；向量性能 harness 不测 BM25/混合相关性（无 nDCG）。两者必须分开做。

---

## 1. 质量线：BEIR + Pyserini

### 1.1 BEIR 是什么（事实标准）
- 18 个零样本数据集、9 类检索任务（事实核查、引用预测、重复问题、论点、新闻、QA、推文、生物医学 IR、实体）。
- 评测 10 个系统、5 个架构族（lexical / sparse / dense / late-interaction / re-ranking）。
- **主指标 nDCG@10**（headline），辅以 Recall@100；用**官方 TREC eval 工具**计算、跨数据集宏平均。
- 来源：BEIR 原论文 arXiv:2104.08663；榜单论文 arXiv:2306.07471。

### 1.2 可引用的核心结论（fastsearch 的融合必须超越的对照）
- **BM25 是极强的零样本基线**；最早的稠密检索器整体**打不过** BM25（最佳稠密模型 TAS-B 仅在 18 个数据集里的 8 个上胜出）。
- 稀疏模型的跨域泛化优于稠密。
- Rerank / late-interaction 平均最好，但算力代价高；高效稠密/稀疏检索常不及 BM25。
- ⚠️ **时效性**：此结论出自 2021 年。E5/BGE 等新嵌入已大幅缩小零样本差距（Pyserini 自家 trec-covid 上 BGE 0.781 已 > BM25 0.595）。**用当前 Pyserini 基线，别用 2021 论文的旧数字。**

### 1.3 Pyserini「两键复现」矩阵（直接抄基线）
- castorini/pyserini 维护 18 个 BEIR 数据集的回归矩阵，每个含 BM25 flat/multifield、SPLADE、Contriever、BGE 的 **nDCG@10 + R@100**，预建索引托管在 HuggingFace。
- 页面：<https://castorini.github.io/pyserini/2cr/beir.html>
- 样例基线：trec-covid 上 **BM25 = 0.595 vs BGE = 0.781**（nDCG@10）。
- ⚠️ fastsearch 是 Rust/Tantivy，**进不去** Pyserini（Python/Lucene）。正确用法：采用相同 collections/qrels/指标，把 Pyserini 公开数字当基线，**自跑 fastsearch 证明 RRF 融合相对纯 BM25 / 纯向量的增益**。
- ⚠️ Pyserini 主要是「单策略」矩阵（BM25 / 稠密 / 学习稀疏分别报），不是开箱即用的 RRF 混合 harness。混合增益要靠你自己跑出来。

### 1.4 fastsearch 落地做法
1. 选 BEIR 子集（建议：trec-covid、nfcorpus、fiqa、hotpotqa、arguana——覆盖长短文/QA/论点）。
2. 用 `fastsearch-eval`（已有 nDCG/recall/MRR + CI 回归门禁）跑三种 mode：`keyword` / `vector` / `hybrid`。
3. 对照列：你的 BM25（Tantivy）vs Pyserini BM25；你的 hybrid vs 纯路；嵌入模型与 Pyserini 对齐（如同跑 BGE）以保可比。
4. 把「hybrid > max(keyword, vector)」做成 CI 断言（`assert_no_regression`）。

---

## 2. 向量性能线

### 2.1 ann-benchmarks（MIT，最权威）
- 40+ ANN 实现，**已含全部竞品**：Milvus、Qdrant、Weaviate、pgvector（及 pgvecto.rs）、Elasticsearch、OpenSearch KNN、FAISS、hnswlib、ScaNN、DiskANN。
- 预切分数据集 + top-100 ground truth：SIFT-128、GloVe-25/50/100/200、Fashion-MNIST-784、GIST-960（HDF5，含 train/test/metric/真值）。
- 强制单 CPU 饱和以隔离算法吞吐；输出 **recall-vs-QPS 前沿图**。
- 来源：github.com/erikbern/ann-benchmarks；论文 arXiv:1807.05614。
- ⚠️ runner 是 Python/Docker，Rust 引擎需写适配器；但 HDF5 数据集+真值可直接复用。

### 2.2 Qdrant vector-db-benchmark（Apache-2.0，接入摩擦最低）
- 已含 client：Elasticsearch、Milvus、OpenSearch、**pgvector**、Qdrant(+hybrid/native)、Redis、Weaviate（9 个，覆盖你 9 个对手里的 6 个）。
- 扩展只需实现 3 个基类 `BaseConfigurator / BaseUploader / BaseSearcher`，注册进 `ClientFactory`——**正好包住 REST index/search 接口**，fastsearch 有 REST API，接入成本最低（见本仓 [adapter 骨架](../benchmarks/qdrant-adapter/)）。
- 方法论可抄：相同 Azure 机器（client D8ls v5 / server D8s v3，8 vCPU）、**内存统一封顶 25 GB** 保公平、含过滤检索场景；指标 RPS / 延迟 / p95 / 索引时间 / 精度。数据集 dbpedia-openai-1M-1536（cosine）、deep-image-96-10M、gist-960-1M（euclidean）、glove-100（angular）。
- ⚠️ 不含 Meilisearch / Typesense / ParadeDB。`qdrant_hybrid` 是 Qdrant 自家稀疏+稠密，不是跨引擎混合质量 harness。

### 2.3 Zilliz VectorDBBench（MIT，带成本指标）
- 覆盖 pgvector、pgvectorscale、Qdrant、Weaviate、Elasticsearch、AWS OpenSearch、Milvus（30+）。
- 指标：QPS、Latency（含 p99：`serial_latency_p99` / `conc_latency_p99`）、Recall、Load 时间、Index 构建时间，外加 **QP$（每美元性能，云成本）**——直接服务 fastsearch 的成本叙事。
- ⚠️ recall 是对向量真值，**不测 BM25/混合相关性**；无内置 fastsearch client，需写适配器。

### 2.4 Weaviate ANN 方法论模板（可抄）
- 数据集选型仿 ann-benchmarks；报 Recall@10/@100、多线程 QPS、mean + P99 延迟（每点 10,000 请求）。
- 来源：docs.weaviate.io/weaviate/benchmarks/ann。

---

## 3. 成本/部署线

- 标准 benchmark 稀缺：除 VectorDBBench 的 **QP$**，业界缺统一口径。
- fastsearch 的差异化（**只需 pgvector + 逻辑复制、零 `shared_preload_libraries` 原生扩展**，可跑任意托管 PG）相对 ParadeDB（需原生扩展）目前只能**论证**、难以用统一数字量化——这是一个公开缺口（见开放问题）。
- 建议：套用 VectorDBBench QP$ + Qdrant 的「相同机器、内存 25GB 封顶」法，在 RDS/Supabase/Neon 上实跑，把「托管可移植」做成可测的部署矩阵（能装上 + 跑通 + 成本/QPS）。

---

## 4. 缺口与诚实记账

1. **无端到端混合质量 harness**：没有现成框架把 BM25+向量+RRF 一起出 nDCG。融合增益只能自跑。
2. **三个对手未被向量 harness 覆盖**：Meilisearch / Typesense / ParadeDB 是全文/混合引擎而非纯 ANN 库，需引用它们各自公开 benchmark 或自建 harness。
3. **三处复用摩擦**：
   - Pyserini 是 Python/Lucene → fastsearch 只能借数据集+基线，不能跑进去。
   - ann-benchmarks / VectorDBBench / Qdrant 框架均 Python/Docker、**无内置 fastsearch client** → 各需小适配器（Qdrant/ann-benchmarks 的 REST/HTTP client 路径最省事）。
   - 没有一个向量性能 harness 测相关性，也没有一个质量 harness 测 QPS/延迟/内存/成本。
4. **✅ 引擎侧入口（已实现）**：REST `/v1/index` 现接收**预计算向量**——chunk 携带 `vector` 字段则直接 `Engine::ingest_vector`、跳过嵌入；`POST /v1/collections` 注册集合 dim/distance 并 introspect 服务端实际向量后端（benchmark 据此确认「被测后端」），ingest 校验预计算向量维度。向量后端选择仍是**服务端级** env（`FASTSEARCH_VECTOR_BACKEND`），按「一进程一配置」跑——per-collection 后端不支持（见 adapter README）。
5. **⚠️ 厂商自测有偏**：Qdrant / Weaviate / Zilliz 公布的**结果数字**都是自家跑的（Zilliz 公开质疑过 Qdrant 的 segment 配置）。**方法论可复用，胜负结论一律自己重跑验证。**

---

## 5. 推荐实验设计（落地清单）

- [ ] **质量**：BEIR 子集上报 nDCG@10 + Recall@100，三 mode 对比，对标 Pyserini 当前 BM25/BGE 基线；CI 断言 hybrid > 单路。
- [ ] **向量性能**：（`/v1/index` 预计算向量入口 ✅、`SearchRequest.ef_search` 逐查询动态调 ✅ 均已就绪）把暴力/HNSW+u8/pgvector 三后端插进 Qdrant 框架 → **固定索引、只扫 `ef_search`** 产出 recall-vs-QPS 前沿，对标 pgvector + 各向量库。HNSW 建图期 `m`/`ef_construct` 若要扫则各建一次索引（扩 env，下一步）。
- [ ] **成本/部署**：VectorDBBench QP$ + Qdrant 25GB 封顶法，在托管 PG 上实跑，量化「pgvector-only」可移植性。
- [ ] **全部自测**，不引用厂商胜负结论；每个数字标注机器/数据集/配置/日期。

---

## 6. 开放问题
1. 是否存在测**端到端混合**（BM25+向量+RRF 一起出 nDCG）的开源 benchmark？
2. Meilisearch / Typesense / ParadeDB 各自如何公布质量与性能数字？有无能把全文引擎与向量库放同一标尺的统一 harness？
3. 给定 fastsearch 的 REST API，最低成本接入路径是 ann-benchmarks wrapper、Qdrant client、还是 VectorDBBench client？哪个最支持过滤检索 + u8量化/HNSW vs pgvector-direct 后端对比？
4. 成本/部署维度除 QP$ 外有无标准 benchmark，能量化「零原生扩展」相对 ParadeDB 的可移植优势？

## 来源
- BEIR：arXiv:[2104.08663](https://arxiv.org/pdf/2104.08663)、arXiv:[2306.07471](https://arxiv.org/pdf/2306.07471)
- Pyserini 2CR：<https://castorini.github.io/pyserini/2cr/beir.html>、<https://github.com/castorini/pyserini>、SIGIR'26 arXiv:2509.02558
- ann-benchmarks：<https://github.com/erikbern/ann-benchmarks>、<https://ann-benchmarks.com/>、arXiv:1807.05614
- Qdrant：<https://qdrant.tech/benchmarks/>、<https://github.com/qdrant/vector-db-benchmark>
- VectorDBBench：<https://github.com/zilliztech/VectorDBBench>
- Weaviate：<https://docs.weaviate.io/weaviate/benchmarks/ann>
