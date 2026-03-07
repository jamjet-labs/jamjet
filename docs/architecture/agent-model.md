# Agent Model Architecture

JamJet treats agents as **first-class runtime entities** — not just model+prompt wrappers. This document describes the internal agent architecture.

---

## Agent Identity

Every agent has:
- **URI** — `jamjet://org/agent-name` (globally addressable)
- **Agent Card** — machine-readable capability manifest (aligned with A2A spec)
- **Registry entry** — tracked in the agent registry with lifecycle state
- **Version** — agents are versioned; running executions pin to a version

---

## Agent Card Schema

```yaml
agent:
  id: research-analyst
  uri: jamjet://myorg/research-analyst
  name: Research Analyst
  description: Performs deep research and produces structured reports
  version: 1.2.0

  capabilities:
    skills:
      - name: deep_research
        description: Research any topic and produce a structured report
        input_schema: schemas.ResearchQuery
        output_schema: schemas.ResearchReport
      - name: fact_check
        input_schema: schemas.Claim
        output_schema: schemas.FactCheckResult

    protocols:
      - mcp_server     # Can serve tools via MCP
      - mcp_client     # Can consume MCP tools
      - a2a            # Can communicate via A2A

    tools_provided:
      - search_web
      - analyze_document

    tools_consumed:
      - vector_search
      - sql_query

  autonomy: bounded_autonomous
  max_iterations: 20
  token_budget: 50000

  auth:
    type: bearer_token
    scopes: [read, execute]
```

---

## Agent Lifecycle

```
  registered
      │
      ▼
   active ──────────────┐
      │                 │ (pause request)
      │                 ▼
      │              paused
      │                 │ (resume)
      │                 └─────────────────┐
      │                                   │
      │◀──────────────────────────────────┘
      │
      │ (deactivate)
      ▼
  deactivated
      │
      │ (archive)
      ▼
  archived
```

### Lifecycle transitions

| Transition | Trigger |
|-----------|---------|
| registered → active | `POST /agents/{id}/activate` |
| active → paused | `POST /agents/{id}/pause` |
| paused → active | `POST /agents/{id}/resume` |
| active → deactivated | `POST /agents/{id}/deactivate` (drains in-flight tasks) |
| deactivated → archived | Automatic after retention window, or explicit |

---

## Autonomy Levels

| Level | Behavior | Default budget |
|-------|----------|---------------|
| `deterministic` | Pure graph execution, no self-direction | N/A |
| `guided` | Follows graph; chooses tools/approaches per step | Default |
| `bounded_autonomous` | Self-directs within iteration, token, cost limits | Configurable |
| `fully_autonomous` | Free operation with guardrails only | Requires explicit policy |

Autonomy level is enforced at runtime. An agent attempting to exceed its iteration or cost budget triggers an escalation event — routed to a supervisor agent or human approval node.

---

## Agent Registry

The local agent registry (`jamjet-agents` crate) maintains:

```
AgentRegistry
  ├── register(AgentCard) → AgentId
  ├── get(AgentId) → Agent
  ├── find(skill, protocol) → Vec<Agent>
  ├── discover_a2a(url) → Agent          # fetches remote Agent Card
  ├── heartbeat(AgentId)
  └── deactivate(AgentId)
```

In v1 the registry is local (Postgres-backed). In v2, federation across JamJet instances via A2A Agent Card exchange.

---

## Hot Reload

When an agent definition is updated:
- All **new** executions use the new version
- All **running** executions continue on the pinned version
- The registry tracks both versions simultaneously until old-version executions drain
- `jamjet agents inspect <id>` shows which version each running execution is pinned to
