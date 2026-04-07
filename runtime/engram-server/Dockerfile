# ── Stage 1: Build the engram-server binary ────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY runtime/ runtime/
COPY rust-toolchain.toml .

WORKDIR /build/runtime
RUN cargo build --release --bin engram --package jamjet-engram-server

# ── Stage 2: Minimal runtime image ─────────────────────────────────
FROM debian:bookworm-slim

# OCI labels for ghcr.io auto-linking and metadata
LABEL org.opencontainers.image.title="Engram"
LABEL org.opencontainers.image.description="Durable memory layer for AI agents — fact extraction, hybrid retrieval, temporal knowledge graph, and consolidation engine. MCP stdio + REST API."
LABEL org.opencontainers.image.source="https://github.com/jamjet-labs/jamjet"
LABEL org.opencontainers.image.url="https://jamjet.dev"
LABEL org.opencontainers.image.documentation="https://docs.jamjet.dev"
LABEL org.opencontainers.image.licenses="Apache-2.0"
LABEL org.opencontainers.image.vendor="JamJet Labs"

# MCP Registry name annotation (required by registry.modelcontextprotocol.io)
LABEL io.modelcontextprotocol.server.name="io.github.jamjet-labs/engram-server"

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --home-dir /home/engram --shell /bin/false engram \
    && mkdir -p /data \
    && chown engram:engram /data

COPY --from=builder /build/runtime/target/release/engram /usr/local/bin/engram

USER engram
WORKDIR /home/engram

# Database location can be overridden via ENGRAM_DB_PATH or --db flag
ENV ENGRAM_DB_PATH=/data/engram.db
VOLUME ["/data"]

# Default to MCP stdio mode (most common use case for AI agents)
ENTRYPOINT ["engram"]
CMD ["serve", "--db", "/data/engram.db"]
