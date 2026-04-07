# Contributing to JamJet

Thank you for your interest in contributing to JamJet. This document covers how to set up the project locally, the development workflow, and how we make decisions together.

---

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Ways to Contribute](#ways-to-contribute)
- [Development Setup](#development-setup)
- [Repository Structure](#repository-structure)
- [Running Tests](#running-tests)
- [Submitting Changes](#submitting-changes)
- [AI-Assisted Contributions](#ai-assisted-contributions)
- [RFC Process](#rfc-process)
- [Architecture Decision Records](#architecture-decision-records)
- [Code Style](#code-style)
- [Release Process](#release-process)

---

## Code of Conduct

JamJet follows the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). By participating, you agree to uphold it.

---

## Ways to Contribute

- **Bug reports** — open a GitHub issue using the bug report template
- **Feature requests** — open a GitHub issue using the feature request template
- **Documentation** — fix typos, improve explanations, add examples
- **Good first issues** — look for the `good first issue` label
- **RFCs** — propose major changes via the RFC process (see below)
- **Code** — fix bugs, implement features, improve performance, add tests

---

## Development Setup

### Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Rust | stable | `rustup` |
| Python | 3.11+ | `pyenv` recommended |
| PostgreSQL | 15+ | local or Docker |
| `uv` | latest | `pip install uv` |
| `just` | latest | `cargo install just` |

### Clone and bootstrap

```bash
git clone https://github.com/jamjet-labs/jamjet.git
cd jamjet

# Bootstrap all tools
just setup
```

The `just setup` command will:
- Install Rust toolchain via `rust-toolchain.toml`
- Install Python dependencies via `uv`
- Start a local Postgres instance (via Docker if available)
- Run initial DB migrations
- Run a smoke test

### Manual setup (if `just` is not available)

```bash
# Rust dependencies
cargo build

# Python SDK
cd sdk/python
uv sync --all-extras
cd ../..

# Database (local Postgres or SQLite for local dev)
# For local dev, SQLite is used automatically by `jamjet dev`
```

---

## Repository Structure

```
jamjet/
  runtime/              # Rust workspace — the execution core
    core/               # State machine, execution model
    ir/                 # Canonical intermediate representation
    scheduler/          # Node dispatch, leasing, backpressure
    state/              # Event log, snapshots, storage backends
    workers/            # Worker process and task execution
    api/                # REST/gRPC control plane
    telemetry/          # Tracing, metrics, OpenTelemetry
    timers/             # Durable timers and cron
    policy/             # Policy engine
    protocols/          # Protocol adapter trait
      mcp/              # MCP client/server
      a2a/              # A2A client/server
    agents/             # Agent registry, lifecycle, cards
  sdk/python/           # Python SDK and CLI
    jamjet/
      workflow/         # Workflow builder and decorators
      agents/           # Agent definitions
      tools/            # Tool decorators
      models/           # Model adapters
      memory/           # Memory and retrieval
      policies/         # Policy bindings
      protocols/        # MCP/A2A Python helpers
      cli/              # `jamjet` CLI (Typer)
  proto/                # Protobuf definitions
  examples/             # Example projects
  docs/                 # Documentation
    architecture/       # Architecture deep-dives
    rfcs/               # Request for Comments
    guides/             # User guides
    adr/                # Architecture Decision Records
  tests/                # Integration and chaos tests
  benchmarks/           # Performance benchmarks
```

---

## Running Tests

### All tests

```bash
just test
```

### Rust tests only

```bash
cargo test --workspace
```

### Python SDK tests only

```bash
cd sdk/python
uv run pytest
```

### Integration tests (requires Postgres)

```bash
just test-integration
```

### Specific test categories

```bash
# Durability tests (crash recovery)
just test-durability

# MCP conformance tests
just test-mcp

# A2A conformance tests
just test-a2a

# Property tests (scheduler correctness)
cargo test --package jamjet-scheduler -- --test-threads 1
```

### Running examples

```bash
# Start local runtime
jamjet dev

# In another terminal
jamjet run examples/basic-tool-flow
jamjet run examples/rag-assistant
```

---

## Submitting Changes

### For small changes (docs, bug fixes)

1. Fork the repo
2. Create a branch: `git checkout -b fix/my-fix`
3. Make your change
4. Run `just lint` and `just test`
5. Open a pull request

### For significant changes (new features, new node types, protocol changes)

1. **Open an issue first** to discuss the approach
2. If the change is architectural, write an RFC (see below)
3. Get early feedback before investing in a large PR
4. Once aligned, implement and open a PR

### PR guidelines

- Keep PRs focused — one logical change per PR
- Write or update tests for your change
- Update relevant documentation
- Reference any related issues or RFCs
- Ensure CI passes before requesting review
- Add a clear description of what changed and why

### Commit message format

```
<type>(<scope>): <short description>

<optional body>

<optional footer>
```

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`

Examples:
```
feat(scheduler): add backpressure limit per queue type
fix(mcp): handle reconnect on stdio transport disconnect
docs(guides): add MCP tool discovery example
test(a2a): add conformance test for input-required state
```

---

## AI-Assisted Contributions

JamJet is an AI agent runtime — it would be strange to ban AI-assisted contributions. We welcome them, with one rule:

**Disclose AI assistance in the PR description.** A single line is enough:

> *Disclosure: This contribution was developed with AI assistance (Claude / Cursor / Copilot / etc.).*

The bar for accepting a contribution is the same regardless of how it was produced:

1. The change solves a real problem clearly stated in the issue or PR description
2. The code follows existing patterns in the affected module
3. Tests are included for any non-trivial change
4. The contributor responds to review feedback and iterates as needed
5. CI passes

What we will NOT accept, regardless of authorship:

- High-volume drive-by PRs with no engagement after submission
- PRs that don't run tests locally before pushing
- PRs that ignore review feedback
- PRs that introduce dependencies, license changes, or design shifts without discussion in an issue first
- PRs that add features, refactor unrelated code, or "improve" beyond what was asked

In short: be a good collaborator. Use AI to ship faster, not to ship lower-quality contributions. We will treat your PR exactly the same whether you wrote every line by hand or generated it with an agent — the code is what we're reviewing.

If you're unsure whether a change is in scope, open an issue first.

---

## RFC Process

JamJet uses RFCs (Request for Comments) for significant design decisions. An RFC is required when:

- Adding a new node type
- Changing the IR schema
- Adding a new protocol adapter
- Changing the agent card spec
- Modifying storage schemas
- Any change to public SDK or API contracts

### RFC workflow

1. Copy `docs/rfcs/RFC-000-template.md` to `docs/rfcs/RFC-NNN-short-title.md`
2. Fill in the template
3. Open a PR with the RFC document (no code yet)
4. RFC is discussed in the PR; author iterates
5. Once accepted, implementation PR(s) can reference the RFC

See [docs/rfcs/](docs/rfcs/) for all current RFCs.

---

## Architecture Decision Records

Major past decisions are documented as ADRs in [docs/adr/](docs/adr/). When a significant architectural decision is made, add an ADR. ADRs are never deleted — superseded ADRs are marked as such.

ADR format follows the [MADR](https://adr.github.io/madr/) template.

---

## Code Style

### Rust

- `cargo fmt` — formatting (enforced in CI)
- `cargo clippy -- -D warnings` — lints (enforced in CI)
- Prefer explicit error types over `Box<dyn Error>`
- Use `tracing` for all log output, not `println!`
- Every public API should have doc comments

### Python

- `ruff format` — formatting (enforced in CI)
- `ruff check` — lints (enforced in CI)
- `mypy` — type checking (enforced in CI)
- Use `pydantic` models for all structured data
- Use `async`/`await` throughout; no blocking I/O in async paths

### General

- No secrets in code or tests — use env vars
- Tests must be hermetic — no real network calls without explicit opt-in
- Prefer explicit over implicit

---

## Release Process

JamJet uses [semantic versioning](https://semver.org).

- Releases are cut from `main`
- `CHANGELOG.md` is updated as part of the release PR
- Rust crates are published to crates.io
- Python SDK is published to PyPI

For maintainers, see the internal release runbook in `docs/maintainers/release.md`.
