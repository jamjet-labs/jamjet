# ── Stage 1: Build Rust binary ──────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY runtime/ runtime/
COPY rust-toolchain.toml .

WORKDIR /build/runtime
RUN cargo build --release --bin jamjet-server

# ── Stage 2: Final image ────────────────────────────────────────────
FROM python:3.11-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Rust binary
COPY --from=builder /build/runtime/target/release/jamjet-server /usr/local/bin/jamjet-server

# Python SDK
COPY sdk/python /tmp/sdk
RUN pip install --no-cache-dir /tmp/sdk && rm -rf /tmp/sdk

ENV JAMJET_BIND=0.0.0.0
ENV JAMJET_PORT=7700
ENV RUST_LOG=info

EXPOSE 7700

ENTRYPOINT ["jamjet-server"]
