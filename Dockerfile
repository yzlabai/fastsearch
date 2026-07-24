# fastsearch-server 容器镜像（多阶段：构建 → 精简运行时）。
# 构建：docker build -t fastsearch-server .
# 国内网络：docker build --build-arg USE_CN_MIRRORS=true -t fastsearch-server .
# 运行：docker run -p 8642:8642 -e FASTSEARCH_KEYS="dev=:public" -v fsdata:/data fastsearch-server

ARG USE_CN_MIRRORS=false
ARG CN_MIRRORS_ENABLED=${USE_CN_MIRRORS/false/}
ARG CN_IMAGE_PREFIX=${CN_MIRRORS_ENABLED:+docker.m.daocloud.io/library/}

# ---- 构建阶段 ----
FROM ${CN_IMAGE_PREFIX}rust:1.88-slim AS builder

ARG USE_CN_MIRRORS
RUN case "$USE_CN_MIRRORS" in \
      false) ;; \
      true) \
        mkdir -p /usr/local/cargo \
        && printf '%s\n' \
          '[source.crates-io]' \
          'replace-with = "mirror"' \
          '[source.mirror]' \
          'registry = "sparse+https://rsproxy.cn/index/"' \
          > /usr/local/cargo/config.toml \
        ;; \
      *) echo "USE_CN_MIRRORS must be 'true' or 'false'" >&2; exit 2 ;; \
    esac

WORKDIR /build
# 注：当前**无独立依赖预热层**——manifest 与源码一起 COPY，任何源码改动都会重编依赖。
# 若需依赖缓存加速，改用 cargo-chef（recipe.json 层）或 dummy-src 预构建（下一迭代）。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY vendor ./vendor
# 仅构建 server 二进制（release，lto=thin）。.dockerignore 已排除 target/ 等，避免巨大 build context。
RUN cargo build --release -p fastsearch-server --bin fastsearch-server

# ---- 运行时阶段 ----
FROM ${CN_IMAGE_PREFIX}debian:bookworm-slim AS runtime

ARG USE_CN_MIRRORS
# CA 证书供 HTTP 嵌入后端 TLS 使用；curl 供编排层健康检查使用。
RUN if [ "$USE_CN_MIRRORS" = "true" ]; then \
      sed -i \
        -e 's|http://deb.debian.org/debian|http://mirrors.aliyun.com/debian|g' \
        -e 's|http://security.debian.org/debian-security|http://mirrors.aliyun.com/debian-security|g' \
        /etc/apt/sources.list.d/debian.sources; \
    fi \
    && apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 fastsearch \
    && mkdir -p /data && chown fastsearch /data
COPY --from=builder /build/target/release/fastsearch-server /usr/local/bin/fastsearch-server
USER fastsearch
ENV FASTSEARCH_DATA=/data \
    FASTSEARCH_PORT=8642
EXPOSE 8642
VOLUME ["/data"]
# 存活探针走 HTTP GET /healthz（由编排层定义：compose healthcheck / K8s httpGet probe）。
ENTRYPOINT ["/usr/local/bin/fastsearch-server"]
