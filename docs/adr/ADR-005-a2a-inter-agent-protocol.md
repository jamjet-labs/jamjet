# ADR-005: A2A as Default Inter-Agent Protocol

| Field | Value |
|-------|-------|
| Status | Accepted |
| Date | 2026-03-07 |

---

## Context

How should JamJet agents communicate with agents running on other frameworks or organizations?

## Decision

**A2A (Agent-to-Agent) protocol is the default inter-agent protocol for cross-framework and cross-organization communication.** Internal JamJet-to-JamJet agent calls may use an optimized internal protocol, with A2A at the boundary.

## Rationale

- **Open standard** — A2A is a Google-led open protocol with growing adoption
- **Cross-framework** — works with any A2A-compliant agent regardless of underlying framework
- **Cross-org** — enables federated agent networks across organizations
- **Rich semantics** — Agent Cards, task lifecycle, streaming, push notifications, multi-turn
- **Auth support** — bearer, OAuth2, mTLS in the spec

## Alternatives Considered

### Custom JamJet inter-agent API
Pros: maximum optimization for JamJet-to-JamJet.
Cons: ecosystem isolation — external agents cannot participate without JamJet-specific adapters.

### gRPC for all inter-agent
Pros: high performance, strongly typed.
Cons: not an open agent standard; requires schema sharing and gRPC support on both sides.

## Consequences

- JamJet must implement A2A client and server (Phase 2)
- All inter-agent communication must go through protocol adapters — no direct coupling
- mTLS for cross-org federation deferred to Phase 4
