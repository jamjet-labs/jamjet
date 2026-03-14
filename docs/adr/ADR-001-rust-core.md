# ADR-001: Rust for Runtime Core

| Field | Value |
|-------|-------|
| Status | Accepted |
| Date | 2026-03-07 |
| Deciders | JamJet Core Team |

---

## Context

The JamJet runtime must handle high-concurrency scheduling, durable state transitions, worker coordination, and low-latency node dispatch — all while remaining memory-safe and easy to reason about under load. The language choice for the runtime core fundamentally shapes performance ceilings, deployment complexity, and long-term reliability.

## Decision

**Use Rust for the runtime core** — the scheduler, state engine, event log, worker coordination, timers, and API server.

## Rationale

- **Performance** — Rust's zero-cost abstractions and Tokio async runtime handle 10k+ concurrent workflows without GC pauses
- **Memory safety** — eliminates entire classes of runtime bugs (null deref, use-after-free, data races) that would compromise durability guarantees
- **Concurrency model** — Tokio's async/await with structured concurrency maps well to the scheduler and worker coordination patterns we need
- **Operational predictability** — no GC, no JVM startup, low and predictable memory footprint
- **Ecosystem** — strong crates for everything needed: SQLx, Axum, Tonic, Serde, Tracing, OpenTelemetry

## Alternatives Considered

### Go
Pros: simpler concurrency model, fast compilation, strong ecosystem.
Cons: GC pauses (unacceptable for latency-sensitive lease and timer operations), less expressive type system, slower than Rust for CPU-bound scheduling.

### Python (entire stack)
Pros: fastest iteration, best AI ecosystem alignment.
Cons: GIL limits true concurrency, 10–100x slower than Rust for scheduling hot paths, memory safety not guaranteed.

### Java / JVM
Pros: mature ecosystem, virtual threads (Java 21) help concurrency.
Cons: JVM startup overhead, GC pause variability, memory overhead.

## Consequences

- Python remains the primary **authoring** and **SDK** language — the clean service boundary means Python engineers don't need to write Rust
- Rust engineers needed for runtime contributions — higher bar than Go or Python
- Future TypeScript/Go SDKs are easy: they talk to the REST/gRPC API, no Rust knowledge required (Java SDK shipped; Go SDK planned for Phase 5)
- Build times will be longer than Go/Python; mitigated by incremental compilation and CI caching
