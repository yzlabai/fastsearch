#!/usr/bin/env python3
"""自包含向量对标：fastsearch (HNSW) vs Qdrant，同机/同数据/同指标。

故意**不依赖** qdrant/vector-db-benchmark 框架与外网数据集——用确定性合成数据
（高斯簇，有结构、贴近真实 ANN 行为）+ numpy 暴力 ground truth，离线可复现。
对每个引擎扫 ef_search/hnsw_ef 画 recall@k-vs-QPS，并报 p50/p95 延迟。

前提：fastsearch server 以 FASTSEARCH_VECTOR_BACKEND=hnsw 起在 :8642（key=dev）；
Qdrant 起在 :6333。用法：python3 vector_bench.py [--n 5000] [--q 500] [--dim 64]
"""
import argparse, time, json
import numpy as np
import requests

FS = "http://127.0.0.1:8642"
QD = "http://localhost:6333"
KEY = "dev"
COLL = "bench"


def gen_data(n, q, dim, clusters, noise=0.35, seed=42):
    rng = np.random.default_rng(seed)
    centers = rng.normal(size=(clusters, dim))
    base_c = rng.integers(0, clusters, size=n)
    base = centers[base_c] + noise * rng.normal(size=(n, dim))
    qry_c = rng.integers(0, clusters, size=q)
    qry = centers[qry_c] + noise * rng.normal(size=(q, dim))
    norm = lambda m: m / np.linalg.norm(m, axis=1, keepdims=True)
    return norm(base).astype(np.float32), norm(qry).astype(np.float32)


def ground_truth(base, qry, k):
    # 余弦 = 归一化后点积；argpartition 取 top-k。
    sims = qry @ base.T
    idx = np.argpartition(-sims, k, axis=1)[:, :k]
    return [set(row.tolist()) for row in idx]


def recall_at_k(got, truth, k):
    hit = sum(len(set(g) & t) for g, t in zip(got, truth))
    return hit / (k * len(truth))


def pctl(xs, p):
    return float(np.percentile(np.array(xs), p))


# ---- fastsearch ----
def fs_index(base):
    # 注意：/v1/index 是 **doc 级替换**（同 doc_id 的旧 chunk 全删再插）。故每批用**独立
    # doc_id**，否则后批会替换前批、只剩最后一批。chunk_id 仍取全局序号 → citation 末段=全局 id。
    h = {"x-api-key": KEY, "content-type": "application/json"}
    B = 500
    for s in range(0, len(base), B):
        doc = f"d{s}"
        chunks = [
            {"doc_id": doc, "chunk_id": int(s + i), "kind": "paragraph", "text": "",
             "page": 1, "bbox": {"x0": 0, "y0": 0, "x1": 1, "y1": 1}, "char_len": 0,
             "vector": v.tolist()}
            for i, v in enumerate(base[s:s + B])
        ]
        r = requests.post(f"{FS}/v1/index", headers=h,
                          data=json.dumps({"collection": COLL, "doc_id": doc, "chunks": chunks}))
        r.raise_for_status()


def fs_search(qry, k, ef):
    h = {"x-api-key": KEY, "content-type": "application/json"}
    got, lat = [], []
    for v in qry:
        body = {"collection": COLL, "query": "", "mode": "vector",
                "vector": v.tolist(), "top_k": k, "ef_search": ef}
        t = time.perf_counter()
        r = requests.post(f"{FS}/v1/search", headers=h, data=json.dumps(body))
        lat.append((time.perf_counter() - t) * 1000)
        r.raise_for_status()
        ids = {int(hit["citation_id"].split(":")[-1]) for hit in r.json()["hits"]}
        got.append(ids)
    return got, lat


# ---- qdrant ----
def qd_index(base, dim):
    requests.delete(f"{QD}/collections/{COLL}")
    requests.put(f"{QD}/collections/{COLL}", json={
        "vectors": {"size": dim, "distance": "Cosine"}}).raise_for_status()
    B = 500
    for s in range(0, len(base), B):
        pts = [{"id": int(s + i), "vector": v.tolist()} for i, v in enumerate(base[s:s + B])]
        requests.put(f"{QD}/collections/{COLL}/points?wait=true",
                     json={"points": pts}).raise_for_status()


def qd_search(qry, k, ef):
    got, lat = [], []
    for v in qry:
        body = {"vector": v.tolist(), "limit": k, "params": {"hnsw_ef": ef}}
        t = time.perf_counter()
        r = requests.post(f"{QD}/collections/{COLL}/points/search", json=body)
        lat.append((time.perf_counter() - t) * 1000)
        r.raise_for_status()
        got.append({int(p["id"]) for p in r.json()["result"]})
    return got, lat


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=5000)
    ap.add_argument("--q", type=int, default=500)
    ap.add_argument("--dim", type=int, default=64)
    ap.add_argument("--clusters", type=int, default=50)
    ap.add_argument("--noise", type=float, default=0.35)
    ap.add_argument("--k", type=int, default=10)
    a = ap.parse_args()

    print(f"# 合成数据 n={a.n} q={a.q} dim={a.dim} clusters={a.clusters} k={a.k} (seed=42)")
    base, qry = gen_data(a.n, a.q, a.dim, a.clusters, a.noise)
    truth = ground_truth(base, qry, a.k)

    print("索引 fastsearch ...", flush=True)
    t = time.perf_counter(); fs_index(base); fs_build = time.perf_counter() - t
    print("索引 qdrant ...", flush=True)
    t = time.perf_counter(); qd_index(base, a.dim); qd_build = time.perf_counter() - t
    print(f"索引耗时: fastsearch {fs_build:.1f}s | qdrant {qd_build:.1f}s\n")

    efs = [16, 32, 64, 128, 256]
    print(f"{'engine':<11}{'ef':>5}{'recall@'+str(a.k):>11}{'p50(ms)':>10}{'p95(ms)':>10}{'QPS':>9}")
    print("-" * 56)
    rows = []
    for name, fn in [("fastsearch", fs_search), ("qdrant", qd_search)]:
        for ef in efs:
            got, lat = fn(qry, a.k, ef)
            rec = recall_at_k(got, truth, a.k)
            p50, p95 = pctl(lat, 50), pctl(lat, 95)
            qps = 1000.0 / (sum(lat) / len(lat))
            rows.append((name, ef, rec, p50, p95, qps))
            print(f"{name:<11}{ef:>5}{rec:>11.4f}{p50:>10.2f}{p95:>10.2f}{qps:>9.0f}", flush=True)
    return rows


if __name__ == "__main__":
    main()
