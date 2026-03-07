# JamJet development tasks
# Install: cargo install just

# Show available commands
default:
    @just --list

# ── Setup ───────────────────────────────────────────────────────────────
# Bootstrap dev environment (installs git hooks automatically)
setup:
    rustup show
    cd sdk/python && uv sync --all-extras
    just hooks
    @echo "Setup complete. Run 'just dev' to start the local runtime."

# Install git hooks (auto-fmt + clippy on commit)
hooks:
    git config core.hooksPath .githooks
    @echo "Git hooks installed from .githooks/ (pre-commit: cargo fmt + ruff + clippy)"

# ── Development ─────────────────────────────────────────────────────────
# Start local runtime (SQLite mode)
dev:
    cargo run --bin jamjet-server -- --dev

# Build all Rust crates
build:
    cargo build --workspace

# Build release binaries
release:
    cargo build --release --workspace

# ── Testing ─────────────────────────────────────────────────────────────
# Run all tests
test: test-rust test-python

# Run Rust tests
test-rust:
    cargo test --workspace --all-features

# Run Python SDK tests
test-python:
    cd sdk/python && uv run pytest --tb=short

# Run integration tests (requires Postgres)
test-integration:
    cargo test --workspace --all-features --test '*' -- --ignored

# Run durability/crash recovery tests
test-durability:
    cargo test --package jamjet-state --test durability

# Run MCP conformance tests
test-mcp:
    cargo test --package jamjet-mcp

# Run A2A conformance tests
test-a2a:
    cargo test --package jamjet-a2a

# Run property tests (slower)
test-property:
    cargo test --package jamjet-scheduler -- --include-ignored

# ── Linting ─────────────────────────────────────────────────────────────
# Run all lints
lint: lint-rust lint-python

# Rust format + clippy
lint-rust:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-features -- -D warnings

# Python format + lint + typecheck
lint-python:
    cd sdk/python && uv run ruff format --check .
    cd sdk/python && uv run ruff check .
    cd sdk/python && uv run mypy jamjet

# Auto-fix formatting
fmt:
    cargo fmt --all
    cd sdk/python && uv run ruff format .
    cd sdk/python && uv run ruff check --fix .

# ── Database ─────────────────────────────────────────────────────────────
# Run DB migrations (requires DATABASE_URL)
migrate:
    cargo run --bin jamjet-migrate

# Create a new migration
migration name:
    cd runtime/state && cargo run --bin create-migration -- {{name}}

# ── Documentation ────────────────────────────────────────────────────────
# Serve docs locally
docs:
    @echo "Docs are in docs/ — open docs/README.md or use a markdown server"

# Generate Rust API docs
doc:
    cargo doc --workspace --no-deps --open

# ── Examples ─────────────────────────────────────────────────────────────
# Run basic tool flow example
example-basic:
    cd examples/basic-tool-flow && jamjet run workflow.yaml

# Run RAG assistant example
example-rag:
    cd examples/rag-assistant && jamjet run workflow.yaml

# ── CI ───────────────────────────────────────────────────────────────────
# Run the full CI suite locally (what CI runs)
ci: lint test build
    @echo "All CI checks passed."
