# Changelog

All notable changes to JamJet will be documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
JamJet uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

---

## 0.10.0 — 2026-06-12

### SDK

- `jamjet approvals EXECUTION_ID` — table of pending (node_id, tool_name, approver) and decided (node_id, status, user_id, comment) entries; friendly empty-state message when nothing is on record.
- `jamjet approve EXECUTION_ID --decision approved|rejected` — posts to the runtime approve endpoint, prints resolved node_id on success; `--node-id` and `--comment` are optional. Validated decision values give a clean parameter error instead of a traceback. HTTP 4xx from the server prints the `error` field and exits 1 without a traceback.
- `client.list_approvals(execution_id)` — GETs `/executions/{id}/approvals`, returns `{"pending": [...], "decided": [...]}`.
- `client.approve()` gains an optional `node_id` parameter; the runtime infers the node when exactly one approval is pending.
- `jamjet --version` reads the installed package version via `importlib.metadata` instead of the hardcoded string.

---

## Runtime 0.4.0 — 2026-06-12

### Added
- Approval/HITL loop end-to-end: nodes gated by `require_approval_for` policy park durably; `POST /executions/:id/approve` resumes or fails them through the normal scheduler path; rejected approvals fail closed with decider and comment recorded in the event log.
- `GET /executions/:id/approvals` returns `{"pending": [...], "decided": [...]}` with node_id, tool_name, approver, context, and sequence for each entry.
- `jamjet_approve` MCP tool routes through the same approve path.
- Validated approve API contract: 409 when nothing is pending or already decided or the execution is terminal; 400 when the node is ambiguous (multiple pending without a node_id); response includes the resolved node_id.
- OTel GenAI semantic conventions emitted on model-node spans (model, system, input/output tokens, finish reason).
- SQLite `BEGIN IMMEDIATE` on read-then-write transactions to prevent SQLITE_BUSY under concurrent worker load.

---

## 0.8.3 — 2026-05-11

### Added
- `jamjet.integrations.openai_guardrail` — JamJet policy as an OpenAI Agents SDK tool guardrail. Same `policy.yaml`, same v1 audit JSONL as `@jamjet/claude-code-hook` / `@jamjet/mcp-shim` / `@jamjet/openai-guardrail` (TS).
- New exceptions: `JamjetPolicyBlocked`, `JamjetApprovalRequired`.

### Notes
- Approval flow surfaces as `JamjetApprovalRequired` exception in v0.1. Full approval flow with `jamjet approve <run-id>` integrates with the OpenAI Agents SDK approval API in v0.2.

---

## 0.8.2 — 2026-05-11

### Added
- `jamjet demo` audit events now emit the v1 portable schema: `adapter: "python-sdk"`, `host: "python"`, `schema_version: 1`, `policy_version: "1"`, `rule_kind`. Plus `args`, `server`, and the `ts` field (replaces `timestamp`).
- Audit events from `jamjet` now share the same JSONL shape as `@jamjet/claude-code-hook`, `@jamjet/mcp-shim`, and `@jamjet/cloud` — `cat ~/.jamjet/audit/<YYYY-MM-DD>/*.jsonl` gives a unified view across every JamJet adapter.

### Changed
- Internal `DemoAuditEvent.timestamp` field renamed to `ts`. The `timestamp` key is preserved on serialized output as an alias for backward compatibility with anything reading jamjet 0.8.1 audit JSON.

### Notes
- Foundation for Phase 2's "one policy, one audit trail" claim. Other JamJet adapters (claude-code-hook, mcp-shim, openai-guardrail) live in [`jamjet-labs/jamjet-policy`](https://github.com/jamjet-labs/jamjet-policy).

---

## [0.8.1] — 2026-05-11

### Added
- `jamjet demo` CLI: 4 zero-setup safety demos — `unsafe-tool-call`, `approval`, `budget-cap`, `mcp-tool-policy`.
  All demos run with no API key, no Docker, no cloud. Mock agent named `DeterministicDemoAgent`. Every output ends with "The model is mocked. The enforcement path is real."
- `--json` flag on every demo emits a machine-readable audit event.
- Audit events written to `.jamjet-demo/runs/<run-id>.json` for inspection.
- Examples 01–04 in `examples/` mirror the demo CLI commands.

### Changed
- PyPI description rewritten from "agent-native runtime and framework" to "safety layer for AI agents".
- README hero rewritten to lead with the 60-second safety demo.

### Notes
- This release is SDK-based. **JamJet Gateway** — an MCP proxy applying the same policy to MCP traffic from Claude Desktop, Cursor, and other MCP clients — is the next major milestone. See [jamjet.dev/gateway](https://jamjet.dev/gateway).

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
