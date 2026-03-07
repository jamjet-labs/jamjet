# ADR-004: MCP as Primary External Tool Protocol

| Field | Value |
|-------|-------|
| Status | Accepted |
| Date | 2026-03-07 |

---

## Context

How should JamJet integrate external tools? Options: custom REST/gRPC tool API, function-calling conventions, or an open standard like MCP.

## Decision

**MCP (Model Context Protocol) is the primary mechanism for external tool integration.** Local Python tools remain first-class, but MCP should be equally ergonomic for external tools.

## Rationale

- **Open standard** — MCP is gaining broad ecosystem adoption (VS Code, Cursor, Claude, many frameworks)
- **Zero-config tool servers** — any MCP server is automatically a tool provider; no manual registration
- **Rich capabilities** — tools, resources, prompts, and sampling in one protocol
- **Ecosystem leverage** — thousands of existing MCP servers (GitHub, filesystem, databases, etc.) work immediately
- **Bidirectional** — JamJet can both consume and serve MCP, enabling IDE and agent composability

## Alternatives Considered

### Custom gRPC tool protocol
Pros: full control, maximum performance.
Cons: ecosystem isolation — nothing outside JamJet can use it without writing a JamJet adapter.

### OpenAI function-calling convention
Pros: familiar to many developers.
Cons: not a transport — still need HTTP/gRPC wrapper; not bidirectional.

## Consequences

- JamJet must implement MCP client (Phase 1) and server (Phase 2) — significant but well-bounded scope
- MCP protocol versioning must be handled carefully (capability negotiation at connection time)
