# 向量对标实测结果 — fastsearch (HNSW) vs Qdrant

> 2026-06-30｜harness：[`vector_bench.py`](vector_bench.py)（自包含、离线可复现）｜机器：本机 macOS（darwin arm64）｜fastsearch **release** 构建、`FASTSEARCH_VECTOR_BACKEND=hnsw`、单线程顺序查询；Qdrant 1.18.2（Docker）。

这是 [benchmark-strategy](../docs/benchmark-strategy.md) 落地的**第一份真实对比数字**（此前只有 adapter 骨架）。**诚实记账**：合成数据、单线程、未调 HNSW 建图参数——是方法学验证与基线，不是「冠军榜」。

## 方法

- **数据**：确定性高斯簇合成（`seed=42`），L2 归一化，余弦距离。无外网数据集依赖。
- **Ground truth**：numpy 暴力全比 top-10。
- **指标**：recall@10（vs 暴力真值）、p50/p95 单查询延迟、QPS（1/平均延迟，顺序单线程）。
- **曲线钮**：fastsearch `ef_search` / Qdrant `hnsw_ef`，同一索引逐查询扫 {16,32,64,128,256}，不重建。
- **公平性**：同机、同合成数据、同 query 集、同 k=10；两端都走本地 HTTP REST。

## 结果

### 配置 A（n=5000, dim=64, clusters=50, noise=0.35）

| engine | ef | recall@10 | p50(ms) | p95(ms) | QPS |
|---|---:|---:|---:|---:|---:|
| fastsearch | 16–256 | **0.991** | ~5.3 | ~6.1 | ~188 |
| qdrant | 16 | 1.000 | 1.09 | 1.62 | 856 |
| qdrant | 256 | 1.000 | 1.59 | 2.08 | 605 |

索引耗时：fastsearch 1.7s / qdrant 0.2s。

### 配置 B（n=10000, dim=96, clusters=20, noise=0.9 — 更难）

| engine | ef | recall@10 | p50(ms) | p95(ms) | QPS |
|---|---:|---:|---:|---:|---:|
| fastsearch | 16–256 | **0.989** | ~7.4 | ~8.6 | ~135 |
| qdrant | 16 | 1.000 | 1.15 | 1.52 | 837 |
| qdrant | 128 | 1.000 | 2.38 | 3.18 | 422 |

索引耗时：fastsearch 3.4s / qdrant 0.4s。

## 解读（诚实）

- **召回有竞争力**：fastsearch HNSW（u8 量化图 + 全精度重排）recall@10 ≈ **0.99**，跨易/难两档稳定。Qdrant 1.0（小规模、f32 精确图）。差距 ~1pt，符合 u8 量化档预期。
- **延迟/吞吐落后 ~5–7×**：fastsearch p50 5–7ms vs Qdrant ~1ms。**主因不是 ANN**：`ef_search` 16→256 对 fastsearch 的 recall 与延迟**几乎无影响**（两档数据一致），说明每查询时间被**混合引擎的请求路径**（全局 `Mutex` 串行 + 融合/RRF 管线 + 命中组装：高亮/引用/分面）主导，ANN 本身只占小头；Qdrant 是专用向量库、路径精简，故 `hnsw_ef` 能清晰拉动延迟。
- **debug vs release 教训**：同配置 debug 构建 fastsearch p50 **53ms**、release **5ms**（~10×）。任何对标必须用 release。

## 可行的优化方向（本测暴露）

1. **每查询固定开销**是当前向量延迟的大头——纯向量模式可走轻量路径（跳过 keyword/融合/部分命中组装）。
2. **`Mutex` 串行**（spec 19 已记）：并发下差距会进一步放大；换 `RwLock`/分片/多副本。
3. HNSW 建图参数（`m`/`ef_construct`）未扫；配合查询分平面量化（spec 15 下一迭代）可再压 ANN。

## 复现

```bash
# 1) Qdrant
docker run -d --name qd -p 6333:6333 qdrant/qdrant:latest
# 2) fastsearch（release + HNSW）
cargo build --release -p fastsearch-server
FASTSEARCH_DATA=$(mktemp -d) FASTSEARCH_KEYS="dev=:" FASTSEARCH_VECTOR_BACKEND=hnsw \
  ./target/release/fastsearch-server &
# 3) 跑（需 numpy + requests）
python3 benchmarks/vector_bench.py --n 5000 --q 500 --dim 64            # 配置 A
python3 benchmarks/vector_bench.py --n 10000 --q 500 --dim 96 --clusters 20 --noise 0.9  # 配置 B
```

> 局限：合成数据非标准集（标准做法见 strategy 的 ann-benchmarks/BEIR 线）；单线程顺序；未扫建图参数。下一步可接 ann-benchmarks HDF5（glove/sift）出标准曲线，并测并发吞吐。
