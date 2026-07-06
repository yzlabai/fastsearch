# fastsearch-server 容器镜像（多阶段：构建 → 精简运行时）。
# 构建：docker build -t fastsearch-server .
# 运行：docker run -p 8642:8642 -e FASTSEARCH_KEYS="dev=:public" -v fsdata:/data fastsearch-server

# ---- 构建阶段 ----
FROM rust:1.88-slim AS builder
WORKDIR /build
# 注：当前**无独立依赖预热层**——manifest 与源码一起 COPY，任何源码改动都会重编依赖。
# 若需依赖缓存加速，改用 cargo-chef（recipe.json 层）或 dummy-src 预构建（下一迭代）。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
# 仅构建 server 二进制（release，lto=thin）。.dockerignore 已排除 target/ 等，避免巨大 build context。
RUN cargo build --release -p fastsearch-server --bin fastsearch-server

# ---- 运行时阶段 ----
FROM debian:bookworm-slim
# 仅需 CA 证书（HTTP 嵌入后端 TLS）；非 root 运行。
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 fastsearch \
    && mkdir -p /data && chown fastsearch /data
COPY --from=builder /build/target/release/fastsearch-server /usr/local/bin/fastsearch-server
USER fastsearch
ENV FASTSEARCH_DATA=/data \
    FASTSEARCH_PORT=8642
EXPOSE 8642
VOLUME ["/data"]
# 存活探针走 HTTP GET /healthz（由编排层定义：compose healthcheck / K8s httpGet probe），
# 运行时镜像不内置 curl，故不在此设 HEALTHCHECK。
ENTRYPOINT ["/usr/local/bin/fastsearch-server"]
