# Core Concepts

This document explains the fundamental concepts in JamJet: agents, workflows, nodes, state, durability, and protocols.

---

## Agent

An **agent** is a first-class runtime entity with:
- A unique **identity** (URI: `jamjet://org/agent-name`)
- An **Agent Card** — machine-readable capability manifest
- A **lifecycle** — registered → active → paused → deactivated
- A **configurable autonomy level** — deterministic / guided / bounded_autonomous / fully_autonomous

Agents are not just model+prompt wrappers. They are addressable, discoverable, and independently deployable units that can communicate via open protocols (MCP, A2A).

---

## Workflow

A **workflow** is a typed directed graph of nodes connected by edges. It:
- Has a shared **typed state** object (Pydantic model)
- Executes **durably** — survives crashes without losing completed steps
- Can **branch** on conditions, **fan out** in parallel, **pause** for humans or timers
- Compiles to a canonical **IR** (Intermediate Representation) before execution

---

## Node

A **node** is a discrete unit of work in a workflow graph. The runtime runs:

| Node type | What it does |
|-----------|-------------|
| `model` | LLM call with a prompt and structured output |
| `condition` | Routes to different next nodes based on expressions |
| `human_approval` | Pauses for a human decision |
| `python_fn`, `java_fn`, `external_fn` | A function run by an external worker (`@tool`, `@workflow.step`, a Java `@Tool`, or any worker on a named queue) |

The IR also has `tool`, `wait`, `parallel`, `join`, `mcp_tool`, `a2a_task`,
`agent` and other kinds the engine has no executor for. Registration refuses a
workflow that uses them. See [Node types](yaml-reference.md#node-types).

---

## State

Every workflow has a **typed state object** — a Pydantic model shared across all nodes.

- Nodes receive the current state as input
- Nodes emit a **patch** (JSON merge patch) describing their changes
- The runtime applies patches transactionally after each node completes
- State is **checkpointed** at every durable boundary

```python
@workflow.state
class ResearchState(BaseModel):
    question: str
    search_results: list[str] | None = None
    final_answer: str | None = None
```

---

## Durability

JamJet workflows are **durable by default**:
- Every node completion is written to an append-only event log
- If the runtime crashes, execution resumes from the last checkpoint
- Human approval pauses survive restarts — they wait indefinitely
- Timers survive restarts — they fire even after a long pause

This is backed by an **event sourcing + snapshot** model. See [State & Durability](../architecture/state-and-durability.md).

---

## MCP (Model Context Protocol)

MCP is the standard protocol for tool integration:
- **As client** — connect to any MCP server and use its tools in your workflow
- **As server** — expose your agent's tools to any MCP client (VS Code, Cursor, other agents)

MCP tool calls are fully durable — checkpointed like any other node.

---

## A2A (Agent-to-Agent)

A2A is the standard protocol for inter-agent communication:
- **As client** — discover external agents via their Agent Card and delegate tasks to them
- **As server** — publish your agent at `/.well-known/agent.json` so any A2A client can find and invoke it

A2A delegation is durable — JamJet tracks the remote task state and resumes correctly after a crash.

---

## Autonomy Levels

| Level | Description |
|-------|-------------|
| `deterministic` | Strict graph execution — agent does exactly what the graph says |
| `guided` | Agent follows the graph but chooses how to approach each step (default) |
| `bounded_autonomous` | Agent can self-direct within budget/iteration constraints |
| `fully_autonomous` | Agent operates freely within guardrails |

The level is a declaration. What the runtime enforces of it, and of the card's constraints, is listed in the [Agent Model Guide](agent-model-guide.md#what-each-constraint-does).
