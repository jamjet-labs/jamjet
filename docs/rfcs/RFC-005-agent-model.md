# RFC-005: Agent Model

| Field | Value |
|-------|-------|
| RFC | 005 |
| Title | Agent Model — Agent Card Spec, Lifecycle, Registry, Autonomy Levels |
| Author(s) | JamJet Core Team |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

This RFC defines JamJet's agent model: what an agent is as a runtime entity, the Agent Card specification, the agent lifecycle state machine, the local agent registry, and the autonomy level system.

---

## Motivation

Most agent frameworks treat an agent as a thin wrapper around a model call. JamJet takes a different position: **an agent is a first-class runtime entity** with identity, capability advertisement, lifecycle management, and configurable autonomy.

This enables:
- Agents that are independently addressable and discoverable
- Multi-agent topologies where agents find and delegate to each other by capability
- Cross-framework interoperability via Agent Cards (aligned with A2A spec)
- Controlled autonomy — from strict orchestration to bounded self-direction

---

## Detailed Design

### 1. Agent Identity

Every agent has:
- **`id`** — unique within a JamJet project (e.g., `research-analyst`)
- **`uri`** — globally addressable (`jamjet://myorg/research-analyst`)
- **`version`** — semver; running executions pin their agent version

### 2. Agent Card Specification

The Agent Card is a machine-readable manifest served at `/.well-known/agent.json` (A2A-aligned) and stored in the agent registry.

```typescript
interface AgentCard {
  id: string;
  uri: string;
  name: string;
  description: string;
  version: string;

  capabilities: {
    skills: Skill[];
    protocols: Protocol[];        // "mcp_server" | "mcp_client" | "a2a"
    tools_provided: string[];
    tools_consumed: string[];
  };

  autonomy: AutonomyLevel;
  constraints?: AutonomyConstraints;

  auth: AuthConfig;
}

interface Skill {
  name: string;
  description: string;
  input_schema: SchemaRef;
  output_schema: SchemaRef;
}

interface AutonomyConstraints {
  max_iterations?: number;
  max_tool_calls?: number;
  token_budget?: number;
  cost_budget_usd?: number;
  allowed_tools?: string[];       // glob patterns
  blocked_tools?: string[];       // glob patterns
  allowed_delegations?: string[]; // agent ids
  require_approval_for?: string[];
  time_budget?: Duration;
}
```

### 3. Agent Lifecycle

```rust
pub enum AgentStatus {
    Registered,
    Active,
    Paused,
    Deactivated,
    Archived,
}
```

State transitions:

| From | To | Trigger |
|------|----|---------|
| `Registered` | `Active` | `POST /agents/{id}/activate` |
| `Active` | `Paused` | `POST /agents/{id}/pause` |
| `Paused` | `Active` | `POST /agents/{id}/resume` |
| `Active` | `Deactivated` | `POST /agents/{id}/deactivate` (drains in-flight) |
| `Deactivated` | `Archived` | Manual or automatic after retention |

Deactivation is **graceful** — the agent stops accepting new tasks but completes all in-flight tasks before transitioning.

### 4. Agent Registry

The local registry is a Postgres-backed store in `jamjet-agents`.

```rust
pub trait AgentRegistry: Send + Sync {
    async fn register(&self, card: AgentCard) -> Result<AgentId>;
    async fn get(&self, id: &AgentId) -> Result<Option<Agent>>;
    async fn get_by_uri(&self, uri: &str) -> Result<Option<Agent>>;
    async fn find(&self, filter: AgentFilter) -> Result<Vec<Agent>>;
    async fn update_card(&self, id: &AgentId, card: AgentCard) -> Result<()>;
    async fn update_status(&self, id: &AgentId, status: AgentStatus) -> Result<()>;
    async fn heartbeat(&self, id: &AgentId) -> Result<()>;
    async fn discover_remote(&self, url: &str) -> Result<Agent>; // fetches Agent Card
}

pub struct AgentFilter {
    pub skill: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<AgentStatus>,
}
```

### 5. Autonomy Levels

```rust
pub enum AutonomyLevel {
    Deterministic,      // strict graph, no self-direction
    Guided,             // follows graph, chooses tools per step (DEFAULT)
    BoundedAutonomous,  // self-directs within constraints
    FullyAutonomous,    // free with guardrails only (requires explicit policy)
}
```

#### Enforcement at runtime

The runtime tracks per-execution:
- `iterations: u32` — incremented per agent cycle
- `tool_calls: u32` — incremented per tool invocation
- `tokens_used: u64` — accumulated from model responses
- `cost_usd: f64` — accumulated from model cost metadata

When any constraint is exceeded:
1. The current node emits a `budget_exceeded` event
2. The scheduler routes to the configured `on_budget_exceeded` handler:
   - `escalate_to_human` — raises a human approval interrupt
   - `escalate_to_supervisor` — delegates to a configured supervisor agent
   - `fail` — marks the node/workflow as failed

### 6. Hot Reload

When an agent's definition is updated (new version):
- Old version remains in the registry with status `active`
- New version is registered with a new version number
- New executions pick up the latest compatible version
- Running executions remain pinned to their version
- Once all old-version executions drain, old version can be deactivated

---

## Drawbacks

- Agent registry adds a database dependency even in simple use cases → mitigated by SQLite local mode
- Version pinning for agents means the registry must track multiple versions simultaneously

---

## Alternatives Considered

### Agents as ephemeral prompt wrappers
Rejected: loses identity, discoverability, and lifecycle management.

### Per-agent database tables
Rejected: too rigid; JSON columns in a single `agents` table give flexibility.

---

## Unresolved Questions

- Should agents be independently deployable in v1? **Recommendation:** No — agents deploy as part of a project. Independent deployment in v2.
- Federation protocol for cross-instance agent discovery: defer to Phase 2 (use A2A Agent Card exchange).

---

## Implementation Plan

See progress-tracker.md Workstream E (Phase 1):
- E.1: Agent Card schema and validation
- E.2: Agent Card validation
- E.3: Local agent registry (SQLite-backed)
- E.4: Agent registration API
- E.5: Agent lifecycle state machine
- E.6: CLI commands
