# Execution Model

The JamJet execution model defines how workflow graphs are represented, scheduled, and executed durably. This document covers the state machine, node types, graph intermediate representation (IR), and scheduling mechanics.

---

## Workflow as a Directed Graph

A JamJet workflow is a **typed directed acyclic graph** (DAG with controlled cycles via explicit loop nodes). Each node represents a discrete unit of work. Edges define the sequencing and conditional routing between nodes.

```
start → fetch_data → analyze → [condition] → approve (human)
                                           → skip_approval
                                               ↓
                                            finalize → end
```

The graph has:
- **Nodes** — typed work units (model call, tool call, condition, human approval, etc.)
- **Edges** — directed transitions, optionally with conditions (`when: output.priority == "high"`)
- **State** — a typed Pydantic model shared across all nodes in the workflow
- **Typed I/O** — each node declares input/output schemas; the runtime validates at boundaries

---

## Workflow State Machine

Each workflow execution moves through a well-defined set of states:

```
           ┌─────────────────────────────────────┐
           │                                     │
     ┌─────▼──────┐    ┌──────────┐    ┌────────▼───────┐
     │  PENDING   │───▶│ RUNNING  │───▶│   COMPLETED    │
     └────────────┘    └──────────┘    └────────────────┘
                            │
                    ┌───────┴───────┐
                    │               │
             ┌──────▼──────┐  ┌────▼────────┐
             │   PAUSED    │  │   FAILED    │
             │ (interrupt) │  └─────────────┘
             └──────┬──────┘
                    │ resume
                    ▼
               RUNNING ...
```

| State | Description |
|-------|-------------|
| `pending` | Workflow created, not yet started |
| `running` | One or more nodes are active or queued |
| `paused` | Waiting for external event, human approval, or timer |
| `completed` | Terminal — all nodes reached end state successfully |
| `failed` | Terminal — a node failed beyond retry policy |
| `cancelled` | Terminal — explicitly cancelled via API |

---

## Node Types

JamJet supports the following node types in v1:

### Core nodes

| Type | Description |
|------|-------------|
| `model` | LLM call with a prompt, model config, and structured output schema |
| `tool` | Local Python function, HTTP endpoint, or gRPC tool service |
| `python_fn` | Arbitrary Python function executed by a Python worker |
| `condition` | Router — evaluates expressions to choose next node(s) |
| `parallel` | Fan-out — spawns multiple child branches concurrently |
| `join` | Waits for all parallel branches to complete before continuing |
| `human_approval` | Pauses workflow for human decision (approve/reject/edit state) |
| `wait` | Suspends until a durable timer fires or an external event is received |
| `subgraph` | Executes a child workflow inline; maps child state to parent |
| `memory_retrieval` | Retrieves documents/context from a retrieval connector |
| `policy` | Evaluates policy rules; can block or branch on violation |
| `finalizer` | Side-effect node (notifications, writes) executed after main logic |

### Protocol nodes (agent-native)

| Type | Description |
|------|-------------|
| `agent` | Delegates to a local JamJet agent as a sub-task |
| `mcp_tool` | Invokes a tool exposed by an external MCP server |
| `a2a_task` | Delegates a task to an external agent via A2A protocol |
| `agent_discovery` | Dynamically discovers and selects an agent at runtime by capability |

---

## Canonical Intermediate Representation (IR)

Every workflow — whether defined in Python or YAML — compiles to the same canonical IR before execution. The IR is serializable to YAML and JSON.

### IR structure

```yaml
workflow_id: report_generation
version: 1.0.0
state_schema: schemas.ReportState

nodes:
  - id: fetch_data
    type: tool
    tool_ref: bigquery.run_query
    input_schema: QueryInput
    output_schema: QueryResult
    retry_policy: default_io
    timeout: 30s

  - id: analyze
    type: model
    model_ref: openai.gpt_x
    prompt_ref: prompts/analyze.md
    output_schema: AnalysisResult
    timeout: 60s

  - id: approve
    type: human_approval
    timeout: 72h

edges:
  - from: fetch_data
    to: analyze
  - from: analyze
    to: approve
    when: "output.confidence < 0.8"
  - from: analyze
    to: end
    when: "output.confidence >= 0.8"
  - from: approve
    to: end

retry_policies:
  default_io:
    max_attempts: 3
    backoff: exponential
    initial_delay: 1s
    max_delay: 30s
    jitter: true
    retryable_on: [io_error, timeout, rate_limit]

observability:
  labels:
    team: analytics
    workflow_type: report
```

### IR validation rules

The compilation layer enforces:

1. All `tool_ref`, `model_ref`, `prompt_ref` resolve to known definitions
2. No unreachable nodes (every non-start node is reachable from `start`)
3. All paths from `start` lead to `end` or a terminal node
4. Cycles are only allowed via explicit loop constructs, not implicit back-edges
5. Node input schema is compatible with the output schema of its predecessor(s)
6. `parallel` and `join` nodes are paired correctly
7. Retry policy references resolve
8. Schema versions are compatible

---

## Scheduling

The scheduler is the heart of the runtime. It runs as a Tokio async loop:

```
loop:
  1. Query event log for runnable nodes
     (all predecessor nodes have node_completed events)
  2. For each runnable node:
     a. Write node_scheduled event (transactional)
     b. Enqueue work item to appropriate queue
  3. Workers pull work items, acquire lease, execute
  4. On success: write node_completed event
  5. On failure: write node_failed event
     → scheduler checks retry policy
     → if retryable: write retry_scheduled, create timer
     → if exhausted: write workflow_failed or route to dead-letter
  6. Wake on new events (event log notification / polling)
```

### Queue types

| Queue | Workload |
|-------|----------|
| `model` | LLM calls — managed concurrency, rate limit aware |
| `tool` | Python tool functions, HTTP tools |
| `retrieval` | Vector search, document retrieval |
| `privileged` | Sensitive tool calls requiring elevated workers |
| `general` | Everything else |

### Lease semantics

- Worker acquires a lease (exclusive lock) on a work item before executing
- Worker must renew heartbeat before lease expiry
- If heartbeat stops, scheduler re-queues the item after lease timeout
- Prevents duplicate execution on worker crash

---

## Durability Boundaries

A **durable boundary** is any point where state is checkpointed before continuing. The runtime guarantees that completed work before a durable boundary is never re-executed (unless explicitly replayed).

Durable boundaries include:
- Node completion
- Interrupt raised (human approval, wait)
- External event receipt
- Retry scheduling
- Child workflow completion
- Timer creation and timer fire

Between durable boundaries, execution may be nondeterministic (e.g., a model call's sampling). This is by design — nondeterminism is confined to task/tool boundaries, not orchestration logic.

---

## Retry Policies

```yaml
retry_policies:
  llm_default:
    max_attempts: 3
    backoff: exponential
    initial_delay: 2s
    max_delay: 60s
    jitter: true
    retryable_on: [rate_limit, timeout, server_error]

  io_default:
    max_attempts: 5
    backoff: exponential
    initial_delay: 1s
    max_delay: 30s
    jitter: true
    retryable_on: [io_error, timeout, connection_reset]

  no_retry:
    max_attempts: 1
```

When `max_attempts` is exhausted, the node moves to `failed` and the item is sent to the dead-letter queue. The workflow can be configured to branch on failure or fail the entire execution.

---

## Timeout Hierarchy

| Timeout type | Scope | Behavior on expiry |
|-------------|-------|--------------------|
| `node_timeout` | Single node execution | Node fails (triggers retry policy) |
| `workflow_timeout` | Entire workflow | Workflow cancelled |
| `heartbeat_timeout` | Worker heartbeat interval | Lease reclaimed, work re-queued |
| `approval_timeout` | Human approval node | Node fails or routes to fallback |
| `schedule_timeout` | Timer/scheduled resume | Timer fires escalation event |

---

## Further Reading

- [State & Durability](state-and-durability.md) — event log internals
- [RFC-001: Execution Model](../rfcs/RFC-001-execution-model.md) — full design proposal
- [RFC-002: IR and Schema System](../rfcs/RFC-002-ir-schema.md) — IR spec details
