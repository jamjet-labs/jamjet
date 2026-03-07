# RFC-002: IR and Schema System

| Field | Value |
|-------|-------|
| RFC | 002 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines the canonical Intermediate Representation (IR) that all workflow definitions compile to, the schema system for typed state and node I/O, and the versioning strategy.

---

## Key Design Points

### IR Structure
Every workflow — whether authored in Python or YAML — compiles to the same canonical IR before submission to the runtime. The IR is a serializable JSON/YAML document containing: workflow metadata, version, state schema reference, node definitions, edge definitions, retry policies, timeout configs, model/tool references, policy bindings, and observability labels.

### Schema System
- State schemas defined via Pydantic models (Python) or JSON Schema (YAML)
- Node input/output schemas declared explicitly or inferred from tool signatures
- Schemas are versioned and stored in a schema registry alongside execution state
- Invalid structured outputs fail predictably with schema validation errors

### Validation Rules
The compilation layer validates:
1. All `tool_ref`, `model_ref`, `prompt_ref` resolve to known definitions
2. Graph connectivity — no unreachable nodes
3. All paths lead to a terminal node
4. Node I/O schema compatibility across edges
5. Retry policy reference validity
6. Schema version compatibility

### Versioning
- Workflows use semver
- Running executions pin exact IR version
- Incompatible schema upgrade → rejected at compile time or migration required

---

## Unresolved Questions
- Schema migration strategy for long-running executions (paused for days) → design doc needed
- Whether to generate Rust types from JSON Schema at build time

---

## Implementation Plan
See progress-tracker.md tasks A.2.1–A.2.4, B.1–B.6.
