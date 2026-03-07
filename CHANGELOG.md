# Changelog

All notable changes to JamJet will be documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
JamJet uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Phase 0 — Architecture & RFC (in progress)
- RFC-001: Execution model
- RFC-002: IR and schema system
- RFC-003: Storage and durability model
- RFC-004: Python SDK design
- RFC-005: Agent model
- RFC-006: MCP integration
- RFC-007: A2A integration
- RFC-008: Protocol adapter framework
- Repository scaffolding (Rust workspace, Python SDK skeleton, CI)

---

## [0.1.0] — TBD

### Phase 1 — Minimal Viable Runtime
- Rust runtime: core state machine, event log, scheduler, worker, REST API
- Python SDK: `@tool`, `@workflow.step`, `@workflow.state`, YAML compiler
- CLI: `jamjet init`, `jamjet dev`, `jamjet run`, `jamjet validate`, `jamjet inspect`
- Storage: SQLite (local), Postgres (production)
- MCP client: stdio and HTTP+SSE transports, tool discovery, tool invocation, `mcp_tool` node
- Agent system: Agent Card schema, local registry, lifecycle management

---

## [0.2.0] — TBD

### Phase 2 — Production Core + Agent Protocols
- Distributed workers with queue isolation and stuck-lease recovery
- Durable timers, cron scheduling, external event resume
- Human approval interrupt node
- MCP server: expose agent tools and resources to external MCP clients
- A2A client: discover and delegate to external A2A agents
- A2A server: publish Agent Card, accept tasks, stream progress
- Protocol adapter framework (`ProtocolAdapter` trait)
- Agent discovery registry
- RBAC light, OpenTelemetry integration, execution replay
