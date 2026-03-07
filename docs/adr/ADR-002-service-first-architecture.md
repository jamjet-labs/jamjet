# ADR-002: Service-First Architecture

| Field | Value |
|-------|-------|
| Status | Accepted |
| Date | 2026-03-07 |

---

## Context

Should the Rust runtime be an embeddable library (like SQLite), a standalone service, or both?

## Decision

**Build service-first.** The Rust runtime runs as a standalone process. The Python SDK communicates with it via REST/gRPC. Direct Rust/Python bindings (via PyO3/maturin) are not used in v1.

## Rationale

- **Polyglot-ready** — TypeScript and Go SDKs can be built with no additional work; they just talk to the same API
- **Clean separation of concerns** — Python authors don't need to know about Rust; Rust engineers don't need to know Python
- **Deployment flexibility** — runtime can be deployed separately, scaled independently, operated as infrastructure
- **Debuggability** — a separate process is easier to inspect, restart, and monitor than an embedded library
- **No FFI complexity** — PyO3 bindings add build complexity, platform-specific packaging issues, and tight coupling

## Local Dev Consideration

For `jamjet dev`, the Python CLI spawns the runtime as a child process automatically. The developer experience is seamless — it feels embedded even though it's a separate process.

## Consequences

- Network hop between Python SDK and runtime (negligible for workflow-level operations)
- Runtime process must be running before SDK calls succeed (handled by `jamjet dev`)
- In v2, PyO3 bindings may be added for latency-critical scenarios; the service-first design doesn't prevent this
