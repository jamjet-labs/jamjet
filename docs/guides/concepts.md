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
- Can **branch** on conditions, **fan out** to several next nodes, and **pause** for a human decision
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
- If the runtime crashes, execution resumes from the last checkpoint, and completed nodes do not run again
- A pending human approval survives restarts. It waits until someone decides or, if the node or policy sets a timeout, until its deadline passes; an expired approval never approves

This is backed by an **event sourcing + snapshot** model. See [State & Durability](../architecture/state-and-durability.md).

---

## MCP (Model Context Protocol)

- **The runtime as an MCP server.** `/mcp` exposes the runtime itself as MCP
  tools: start, inspect, list and cancel executions, read events, approve, and
  list or discover agents. It takes the same tokens and roles as the REST API.
- **Calling MCP tools.** A workflow cannot: registration refuses `mcp_tool`
  nodes. To govern an MCP client's tool calls, use the
  [`@jamjet/mcp-shim`](https://npmjs.com/package/@jamjet/mcp-shim) adapter, or
  call the MCP server from a Python step and check policy there, as in
  [`examples/05-mcp-tool-call`](../../examples/05-mcp-tool-call).

See the [MCP guide](mcp-guide.md).

---

## A2A (Agent-to-Agent)

The runtime can fetch a remote agent's Agent Card and register it
(`POST /agents/discover`, or `jamjet agents discover <url> --register`; `jamjet a2a discover <url>` only prints the card). It does not delegate
tasks over A2A (registration refuses `a2a_task` nodes) and does not serve an
A2A endpoint. See the [A2A guide](a2a-guide.md).

---

## Autonomy Levels

| Level | Description |
|-------|-------------|
| `deterministic` | Strict graph execution — agent does exactly what the graph says |
| `guided` | Agent follows the graph but chooses how to approach each step (default) |
| `bounded_autonomous` | Agent can self-direct within budget/iteration constraints |
| `fully_autonomous` | Agent operates freely within guardrails |

The level is a declaration. What the runtime enforces of it, and of the card's constraints, is listed in the [Agent Model Guide](agent-model-guide.md#what-each-constraint-does).
