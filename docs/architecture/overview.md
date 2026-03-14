# Architecture Overview

JamJet is built as six cooperating layers. This document describes each layer, the data flow between them, and the key design constraints that shape the system.

---

## The Six Layers

```
┌──────────────────────────────────────────────────────────────┐
│  1. Authoring Layer                                           │
│     Python SDK  ·  Java SDK  ·  Go SDK (planned)  ·  YAML    │
└─────────────────────────┬────────────────────────────────────┘
                          │  compile / validate
┌─────────────────────────▼────────────────────────────────────┐
│  2. Compilation & Validation Layer                            │
│     Graph IR  ·  Schema validation  ·  Policy lint           │
│     Agent Card validation  ·  Tool contract validation       │
└─────────────────────────┬────────────────────────────────────┘
                          │  canonical IR
┌─────────────────────────▼────────────────────────────────────┐
│  3. Execution Runtime  (Rust core)                            │
│     Workflow state machine  ·  Scheduler  ·  Event log       │
│     Checkpoint manager  ·  Retry/timer engine                │
│     Worker coordination  ·  Agent lifecycle manager          │
├──────────────────────────────────────────────────────────────┤
│  4. Protocol Layer                                            │
│     MCP Client  ·  MCP Server                                │
│     A2A Client  ·  A2A Server                                │
│     Protocol Adapter Framework  ·  Agent Registry            │
└─────────────────────────┬────────────────────────────────────┘
                          │
┌─────────────────────────▼────────────────────────────────────┐
│  5. Runtime Services                                          │
│     Model provider adapters  ·  Tool execution adapters      │
│     Memory / retrieval connectors  ·  Policy engine          │
│     Observability sinks                                      │
└─────────────────────────┬────────────────────────────────────┘
                          │
┌─────────────────────────▼────────────────────────────────────┐
│  6. Control Plane / APIs                                      │
│     REST + gRPC  ·  Agent registry API  ·  Admin             │
│     MCP server endpoints  ·  A2A server endpoints            │
│     Execution inspection  ·  Resume / replay / cancel        │
└─────────────────────────┬────────────────────────────────────┘
                          │
┌─────────────────────────▼────────────────────────────────────┐
│  Storage                                                      │
│     Postgres (production)  ·  SQLite (local dev)             │
│     Append-only event log  ·  Snapshot store                 │
└──────────────────────────────────────────────────────────────┘
```

---

## Language Boundary

JamJet has a deliberate language split:

| Layer | Language | Rationale |
|-------|----------|-----------|
| Runtime core, scheduler, state engine, worker coordination | **Rust** | Performance, memory safety, async concurrency via Tokio |
| SDKs, CLI, tool definitions, schema authoring | **Python, Java, Go (planned)** | Developer adoption, AI ecosystem compatibility, enterprise reach |
| Wire protocol (internal services) | **Protobuf over gRPC** | Strongly typed, polyglot-ready |
| Control plane API | **REST (JSON) + gRPC** | REST for broad compatibility; gRPC for high-perf internal |

The Python SDK communicates with the Rust runtime via the REST/gRPC control plane API. There are no direct Python↔Rust bindings in v1 — the clean service boundary keeps the architecture polyglot-ready. Java SDK is shipped; Go and TypeScript SDKs are planned for Phase 5.

---

## Data Flow: Workflow Execution

```
Developer writes YAML or Python
        │
        ▼
Python SDK compiles to Canonical IR
        │  (JSON / YAML workflow graph)
        ▼
Compilation layer validates:
  - Graph connectivity (no dangling edges)
  - Schema compatibility (node I/O types)
  - Tool availability (all tool_refs resolve)
  - Policy conformance
        │
        ▼
IR submitted to Runtime API
        │
        ▼
Scheduler picks up workflow_started event
  → detects runnable nodes (all deps satisfied)
  → dispatches work items to queue
        │
        ▼
Worker picks up item from queue
  → executes node (model call / tool call / python fn)
  → emits node_completed (or node_failed) event
        │
        ▼
Event log receives event (append-only, transactional)
        │
        ▼
Scheduler wakes, detects newly runnable nodes
  → repeat until workflow_completed
        │
        ▼
Periodic snapshots compact event log for performance
```

---

## Data Flow: MCP Tool Invocation

```
Workflow node: type=mcp_tool
        │
        ▼
Worker picks up mcp_tool node item
        │
        ▼
MCP Client (jamjet-mcp crate):
  1. Resolves MCP server connection (from config)
  2. Sends tools/call JSON-RPC request
  3. Receives response (typed output)
        │
        ▼
Worker emits node_completed with MCP result
        │
        ▼
Event log checkpoints result (durable)
  → MCP tool call is fully recoverable on crash
```

---

## Data Flow: A2A Agent Delegation

```
Workflow node: type=a2a_task
        │
        ▼
Worker picks up a2a_task node item
        │
        ▼
A2A Client (jamjet-a2a crate):
  1. Fetches Agent Card from /.well-known/agent.json
  2. Submits task via tasks/send JSON-RPC
  3. Tracks lifecycle: submitted → working → completed
  4. If stream=true: consumes SSE stream, updates traces
  5. If input-required: emits interrupt_raised, pauses workflow
        │
        ▼
A2A task state durably tracked in event log
  → crash/restart resumes tracking of remote task
        │
        ▼
On completed: maps artifacts into workflow state
Worker emits node_completed
```

---

## Rust Crate Structure

```
runtime/
  Cargo.toml              # workspace
  core/                   # jamjet-core    — execution states, node types, retry/timeout models
  ir/                     # jamjet-ir      — canonical IR structs, serialization, validation
  scheduler/              # jamjet-scheduler — runnable detection, queue dispatch, leasing
  state/                  # jamjet-state   — event log, snapshots, storage backends (PG + SQLite)
  workers/                # jamjet-worker  — worker process, capability advertisement, lease renewal
  api/                    # jamjet-api     — Axum REST API, gRPC (Tonic)
  telemetry/              # jamjet-telemetry — tracing, metrics, OTel exporters
  timers/                 # jamjet-timers  — durable timers, cron
  policy/                 # jamjet-policy  — policy engine, violation events
  protocols/
    mod.rs                # jamjet-protocols — ProtocolAdapter trait
    mcp/                  # jamjet-mcp     — MCP client + server
    a2a/                  # jamjet-a2a     — A2A client + server
  agents/                 # jamjet-agents  — agent registry, lifecycle, Agent Card parsing
```

---

## Storage Model

JamJet uses a **hybrid event-sourcing + snapshot** model:

- The **event log** is the source of truth — every state transition is an appended event
- **Snapshots** are taken periodically to avoid replaying the entire log on every resume
- Current workflow state = latest snapshot + delta events since that snapshot

This gives:
- Full audit trail (every event is preserved)
- Efficient state restoration (snapshot + short delta)
- Replay from any point in history

Storage backends are trait-abstracted:

| Backend | Use case |
|---------|----------|
| SQLite | Local development (`jamjet dev`) |
| Postgres | Production (v1) |
| FoundationDB | Future cloud-native scale |

---

## Concurrency Model

- Rust runtime uses **Tokio** for async concurrency
- The scheduler runs as an async loop, detecting runnable nodes and dispatching to queues
- Workers are separate processes (or threads), communicating via queued tasks and heartbeats
- Worker leases prevent duplicate execution on worker death
- Queue isolation by workload type: model workers, Python tool workers, retrieval workers, privileged workers

---

## Agent-Native Design

Every agent in JamJet is a first-class runtime entity:

- **Addressable** via URI (`jamjet://org/agent-name`)
- **Discoverable** via Agent Card and registry API
- **Lifecycle-managed** — registered → active → paused → deactivated → archived
- **Protocol-capable** — can advertise MCP server, MCP client, A2A capabilities
- **Autonomy-configurable** — deterministic / guided / bounded_autonomous / fully_autonomous

See [Agent Model Architecture](agent-model.md) for internals.

---

## Further Reading

- [Execution Model](execution-model.md) — state machine and node type details
- [State & Durability](state-and-durability.md) — event log and snapshot internals
- [Agent Model](agent-model.md) — agent-native architecture
- [MCP Architecture](mcp-integration.md) — MCP client and server design
- [A2A Architecture](a2a-integration.md) — A2A protocol implementation
- [Protocol Adapters](protocol-adapters.md) — extensibility model
