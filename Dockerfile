# docparse-rs REST sidecar — multi-stage build.
# Pure-Rust single binary (~26MB); optional model files (OCR/UniRec/layout) are
# NOT baked in — mount them at /app/models at runtime. Digital PDFs need none.
# syntax=docker/dockerfile:1
# REGISTRY: base-image registry prefix. Default empty = Docker Hub. For mirror-only
# networks set e.g. --build-arg REGISTRY=docker.1ms.run/library/ (compose passes it).
ARG REGISTRY=

FROM ${REGISTRY}rust:1-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin docparse

FROM ${REGISTRY}debian:bookworm-slim
# ca-certificates: outbound HTTPS for the optional VLM enhancer (--vlm-url).
# curl: container HEALTHCHECK against /healthz.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/docparse /usr/local/bin/docparse
# Optional models mounted here (e.g. -v ./models:/app/models:ro):
#   models/ppocr (OCR), models/unirec (table/formula), models/layout/*.onnx
EXPOSE 8642
# 0.0.0.0 so other containers on the compose network can reach it (no auth —
# keep it on a private network, never publish the port to the host/internet).
ENTRYPOINT ["docparse"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8642"]
