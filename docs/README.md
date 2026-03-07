# JamJet Documentation

Welcome to the JamJet documentation. Use the table below to find what you need.

---

## Guides — Start here

| Guide | Description |
|-------|-------------|
| [Quickstart](guides/quickstart.md) | Running your first workflow in under 10 minutes |
| [Core Concepts](guides/concepts.md) | Agents, workflows, nodes, state, durability explained |
| [Workflow Authoring](guides/workflow-authoring.md) | YAML and Python authoring — everything you need to know |
| [Python SDK](guides/python-sdk.md) | Full Python SDK reference with examples |
| [YAML Reference](guides/yaml-reference.md) | Complete YAML spec for all files |
| [Agent Model](guides/agent-model-guide.md) | Agent Cards, lifecycle, autonomy levels, discovery |
| [MCP Integration](guides/mcp-guide.md) | Connecting to MCP servers and exposing tools via MCP |
| [A2A Integration](guides/a2a-guide.md) | Delegating to and serving external A2A agents |
| [Human-in-the-Loop](guides/hitl.md) | Approval nodes, state editing, audit logging |
| [Observability](guides/observability.md) | Tracing, replay, debugging, OpenTelemetry |
| [Deployment](guides/deployment.md) | Production deployment — Postgres, workers, scaling |
| [Security](guides/security.md) | Auth, RBAC, secrets, sandboxing, audit |

---

## Architecture

| Document | Description |
|----------|-------------|
| [Overview](architecture/overview.md) | Full system architecture — all six layers |
| [Execution Model](architecture/execution-model.md) | State machine, node types, graph IR |
| [State & Durability](architecture/state-and-durability.md) | Event sourcing, snapshots, crash recovery |
| [Agent Model](architecture/agent-model.md) | Agent-native runtime internals |
| [MCP Architecture](architecture/mcp-integration.md) | MCP client/server implementation design |
| [A2A Architecture](architecture/a2a-integration.md) | A2A client/server implementation design |
| [Protocol Adapters](architecture/protocol-adapters.md) | Extensible protocol layer design |

---

## RFCs — Design Proposals

RFCs document significant design decisions before implementation.

| RFC | Title | Status |
|-----|-------|--------|
| [RFC-001](rfcs/RFC-001-execution-model.md) | Execution Model | Draft |
| [RFC-002](rfcs/RFC-002-ir-schema.md) | IR and Schema System | Draft |
| [RFC-003](rfcs/RFC-003-storage-durability.md) | Storage and Durability Model | Draft |
| [RFC-004](rfcs/RFC-004-python-sdk.md) | Python SDK Design | Draft |
| [RFC-005](rfcs/RFC-005-agent-model.md) | Agent Model | Draft |
| [RFC-006](rfcs/RFC-006-mcp-integration.md) | MCP Integration | Draft |
| [RFC-007](rfcs/RFC-007-a2a-integration.md) | A2A Integration | Draft |
| [RFC-008](rfcs/RFC-008-protocol-adapters.md) | Protocol Adapter Framework | Draft |

See [rfcs/README.md](rfcs/README.md) for the RFC process.

---

## Architecture Decision Records

ADRs capture major past decisions and their rationale.

| ADR | Title | Status |
|-----|-------|--------|
| [ADR-001](adr/ADR-001-rust-core.md) | Rust for Runtime Core | Accepted |
| [ADR-002](adr/ADR-002-service-first-architecture.md) | Service-First Architecture | Accepted |
| [ADR-003](adr/ADR-003-event-sourcing-snapshots.md) | Event Sourcing with Snapshots | Accepted |
| [ADR-004](adr/ADR-004-mcp-primary-tool-protocol.md) | MCP as Primary External Tool Protocol | Accepted |
| [ADR-005](adr/ADR-005-a2a-inter-agent-protocol.md) | A2A as Inter-Agent Protocol | Accepted |

See [adr/README.md](adr/README.md) for the ADR process.
