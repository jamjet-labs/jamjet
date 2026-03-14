# Changelog

All notable changes to JamJet will be documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
JamJet uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## [0.1.0] — 2026-03-08

First public release. JamJet is a performance-first, agent-native runtime for AI agents — built in Rust, authored in Python.

### Runtime Core (Rust)

- **Durable graph execution** — event-sourced state machine; crash the process and execution resumes exactly where it stopped
- **IR (Intermediate Representation)** — typed graph compiled from YAML or Python; both authoring surfaces compile to the same IR
- **Rust async scheduler** — Tokio-based worker pool with distributed lease semantics; prevents double-execution across multiple runtime instances
- **7 node types**: `model`, `tool`, `http`, `branch`, `parallel`, `wait`, `eval`
- **State machine** — typed state schema (JSON Schema), per-node state patches, full event log
- **PostgreSQL + SQLite** — production-grade durable storage with automatic schema migration; SQLite for local dev (zero config)
- **Event sourcing + snapshots** — every state transition recorded; periodic snapshots for fast recovery
- **REST API** — submit executions, poll status, resume waiting nodes, inspect full event timeline
- **OpenTelemetry** — spans for every node execution, GenAI semantic conventions for model calls, Prometheus metrics

### Protocol Layer

- **MCP client** — connect to any MCP server over `stdio` or `http+sse`; tool discovery, typed invocation, auto-retry
- **MCP server** — expose your agent's tools and resources to external MCP clients
- **A2A client** — discover external agents via Agent Cards, delegate tasks, stream SSE progress
- **A2A server** — publish an Agent Card at `.well-known/agent.json`, accept tasks from any A2A-compatible agent, stream events

### Agent System

- **Agent Cards** — machine-readable identity for every agent (id, name, capabilities, input/output schema, endpoint)
- **Agent registry** — local registry for managing and discovering agents
- **Autonomy modes** — `guided`, `supervised`, `autonomous` with configurable cost and iteration limits

### Eval Harness

- **`eval` node type** — inline quality scoring in any workflow; LLM-judge, assertions, latency, cost scorers
- **`on_fail: retry_with_feedback`** — scorer feedback auto-injected into the next model call
- **Python eval runner** — `EvalDataset`, `EvalRunner`, `BaseScorer` for custom scorers
- **`jamjet eval run`** — CLI command for batch eval; JSONL dataset support, rich table output, `--fail-below` for CI gates

### Python SDK

- **Decorator API** — `@workflow`, `@node` for building workflows as Python classes
- **Builder API** — `WorkflowBuilder`, `ModelNode`, `ToolNode`, `BranchNode`, `ParallelNode`, `EvalNode`
- **`JamJetClient`** — async client for submitting, polling, inspecting, and streaming executions
- **Full type stubs** — compatible with mypy strict mode and Pyright

### CLI

- `jamjet init [name]` — scaffold new project or add JamJet to existing project (like `git init`)
- `jamjet dev` — start local runtime with SQLite; zero config
- `jamjet validate <workflow>` — compile and validate a workflow file without running it
- `jamjet run <workflow> --input <json>` — submit and wait for completion
- `jamjet run ... --stream` — stream execution events as they happen
- `jamjet inspect <exec-id>` — full execution state, event timeline, token usage, cost
- `jamjet ls` — list recent executions with status and duration
- `jamjet resume <exec-id>` — resume a waiting or failed execution
- `jamjet cancel <exec-id>` — cancel a running execution
- `jamjet tools list / call` — inspect and test MCP tool servers
- `jamjet agents inspect` — inspect A2A agent cards
- `jamjet eval run` — run a batch eval dataset

### Packaging

- Python wheel bundles the `jamjet-server` Rust binary (maturin) — `pip install jamjet` gives you everything
- Platform wheels: macOS arm64, macOS x86_64, Linux x86_64, Linux aarch64, Windows x86_64
- Published to PyPI via GitHub Actions on version tags

### Developer Experience

- 8 ready-to-run examples in [jamjet-labs/examples](https://github.com/jamjet-labs/examples)
- Full documentation at [jamjet.dev/docs](https://jamjet.dev/docs/quickstart)
- `justfile` for common dev tasks (`just test`, `just lint`, `just build`)

---

## [0.2.0] — Planned

- Go SDK
- TypeScript SDK
- Hosted runtime plane (zero-ops deployment)
- Enhanced policy engine with multi-tenant isolation
- NATS/Kafka queue backend for high-throughput workloads

---

[Unreleased]: https://github.com/jamjet-labs/jamjet/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamjet-labs/jamjet/releases/tag/v0.1.0
