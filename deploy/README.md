# 部署（B7）

fastsearch 部署制品：容器镜像 + 一键 compose + K8s/CloudNativePG 样例。守"托管 PG 可移植"
硬约束——PG 侧只用 **pgvector + 逻辑复制**，无任何需 `shared_preload_libraries` 的原生扩展。

## 当前状态

**2026-07-01 已部署到 K8s test ns**（具体入口/IP 见团队内部 wiki，避免提交到版本库）。

## 制品

| 文件 | 用途 |
|---|---|
| [`Dockerfile`](../Dockerfile) | 多阶段构建 `fastsearch-server`（release+lto），精简 debian 运行时、非 root、`/data` 卷 |
| [`docker-compose.yml`](../docker-compose.yml) | 一键起全栈：pgvector（`wal_level=logical`）+ server（CDC 同步） |
| [`deploy/cloudnativepg.yaml`](cloudnativepg.yaml) | K8s：CloudNativePG 托管 PG（HA 3 副本，仅 pgvector+逻辑复制）+ server Deployment/Service（无状态多副本、httpGet 探针） |

## 快速起（本机）

```bash
docker compose up --build
curl -s localhost:8642/healthz          # ok
# 灌入 + 检索见 README / clients
```

## 引擎无状态、派生可重建

server 副本各自从**复制流/快照重建**派生索引（PG 是真源），故可水平扩展、崩溃即重放恢复。
向量后端默认暴力精确；大规模设 `FASTSEARCH_VECTOR_BACKEND=hnsw`（近似 ANN）。

## 验证状态（诚实记账）

- **Dockerfile / compose / CloudNativePG manifest：已编写**，标准多阶段 + 标准编排，二进制本地
  `cargo build --release -p fastsearch-server` 已验证可编译。
- **容器镜像 build / K8s 实跑：`待运行验证`**（与 §顶部"已部署到 test ns"不矛盾：那是一次**手工**
  部署记录，此处指**自动化镜像 build / CI 未常态验证**）—— 编写当时 Docker Hub registry 不可达（拉
  `rust:1.88-slim` 基镜 EOF，见 [Dockerfile](../Dockerfile)），未能本环境构建镜像；K8s 部署需集群。
  registry/集群可用后按上文 `docker compose up --build` 与 `kubectl apply -f deploy/cloudnativepg.yaml` 验证。
- CDC 闭环本身已在 Docker PG（pgvector + 逻辑复制）真机验证（见 cdc_closed_loop 测试）。
