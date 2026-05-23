# syntax=docker/dockerfile:1.7

# ─── 构建阶段：engine 二进制 + web-ui (wasm) ──────────────────────────
FROM rust:1-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

RUN rustup target add wasm32-unknown-unknown \
 && cargo install --locked trunk

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared-types ./shared-types
COPY engine       ./engine
COPY web-ui       ./web-ui

# engine 二进制（workspace 已 exclude web-ui，不会被 host target 编译）
RUN cargo build --release -p rust_engine

# 前端 wasm
RUN cd web-ui && trunk build --release


# ─── Stage 3: 运行时镜像 ──────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /src/target/release/rust_engine /usr/local/bin/rust_engine
COPY --from=builder /src/web-ui/dist                ./web-ui/dist
# 基线配置（只读）；服务端用 env (APP__SECTION__KEY=...) 覆盖，私钥走 POLY_PRIVATE_KEY
COPY config.toml.example ./config.toml

# Headless 模式：跳过 TUI / Web，仅后台采集 + 下单
ENV HEADLESS=1

ENTRYPOINT ["rust_engine"]
CMD []

# ─── Web 模式（保留备用）────────────────────────────────────────────
# EXPOSE 3000
# ENTRYPOINT ["rust_engine"]
# CMD ["--web", "--host", "0.0.0.0", "--port", "3000"]
