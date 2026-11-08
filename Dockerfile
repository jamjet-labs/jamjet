# ── Stage 1: Build web frontend ─────────────────────────────────────
FROM node:20-bookworm-slim AS web-builder

WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ .
RUN npm run build

# ── Stage 2: Build Rust binary ──────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY runtime/ runtime/
COPY rust-toolchain.toml .
COPY --from=web-builder /build/web/dist web/dist

WORKDIR /build/runtime
RUN cargo build --release --locked --bin jamjet-server

# ── Stage 3: Final image ────────────────────────────────────────────
FROM python:3.12-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Rust binary
COPY --from=builder /build/runtime/target/release/jamjet-server /usr/local/bin/jamjet-server

# Python SDK
COPY sdk/python /tmp/sdk
RUN pip install --no-cache-dir /tmp/sdk && rm -rf /tmp/sdk

# State, the signed audit log and API tokens live in SQLite on /data, and
# authentication is on. Issue a token with
#   docker exec <container> jamjet-server token create --role operator
# For a throwaway demo with no state and no auth, set STORAGE_BACKEND=memory,
# JAMJET_DEV_MODE=1 and JAMJET_DEV_MODE_ALLOW_REMOTE=1, and publish the port
# on 127.0.0.1 only (see docs/guides/deployment.md).
RUN mkdir -p /data
VOLUME /data
ENV DATABASE_URL=sqlite:///data/runtime.db
ENV JAMJET_EMBED_WORKERS=1
ENV JAMJET_BIND=0.0.0.0
ENV JAMJET_PORT=8080
ENV RUST_LOG=info

EXPOSE 8080

ENTRYPOINT ["jamjet-server"]
