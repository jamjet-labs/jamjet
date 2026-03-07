# RFC-001: Execution Model

| Field | Value |
|-------|-------|
| RFC | 001 |
| Title | Execution Model |
| Author(s) | JamJet Core Team |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

This RFC defines the core execution model for JamJet: how workflows are represented as typed directed graphs, how the state machine drives execution, what node types exist, how work is dispatched to workers, and how durability guarantees are enforced at each step.

---

## Motivation

Agent workflows require more than sequential LLM calls. They need:
- **Branching** — route to different paths based on output conditions
- **Parallelism** — fan out to multiple agents or tools concurrently
- **Durability** — survive process crashes without re-running completed steps
- **Human gates** — pause for approval before irreversible actions
- **Retries** — transient failures should not require manual intervention
- **Typed contracts** — nodes must agree on data shapes to prevent silent failures

This RFC establishes the foundational model that all other components build on.

---

## Detailed Design

### 1. Workflow as a Typed Directed Graph

A workflow is a **directed graph** where:
- **Nodes** are discrete units of work (model calls, tool calls, conditions, human approvals, etc.)
- **Edges** are transitions between nodes, optionally conditional
- **State** is a typed shared object threaded through the entire graph
- **Input/output schemas** are declared per node and validated at runtime boundaries

```
start → [fetch_data] → [analyze] → [condition] → [approve] → [finalize] → end
                                               └→ [auto_finalize] → end
```

Cycles are permitted only via explicit `loop` constructs (a loop node with a counter and exit condition). Implicit back-edges that could cause infinite loops are rejected at compile time.

### 2. Workflow State Machine

Each execution instance tracks these states:

```rust
pub enum WorkflowStatus {
    Pending,
    Running,
    Paused,      // waiting: human approval, timer, external event
    Completed,
    Failed,
    Cancelled,
}
```

Transitions are driven by events appended to the event log (see RFC-003).

### 3. Node Types

#### Core nodes

```rust
pub enum NodeKind {
    Model {
        model_ref: String,
        prompt_ref: String,
        output_schema: SchemaRef,
    },
    Tool {
        tool_ref: String,
        input_mapping: ExpressionMap,
        output_schema: SchemaRef,
    },
    PythonFn {
        module: String,
        function: String,
        output_schema: SchemaRef,
    },
    Condition {
        branches: Vec<ConditionalEdge>,
    },
    Parallel {
        branches: Vec<NodeId>,
    },
    Join {
        wait_for: Vec<NodeId>,
        merge_strategy: MergeStrategy,
    },
    HumanApproval {
        description: String,
        timeout: Option<Duration>,
        fallback: Option<NodeId>,
    },
    Wait {
        condition: WaitCondition, // timer | external_event | both
        correlation_key: Option<String>,
        timeout: Option<Duration>,
    },
    Subgraph {
        workflow_ref: String,
        input_mapping: ExpressionMap,
        output_mapping: ExpressionMap,
    },
    MemoryRetrieval {
        connector_ref: String,
        query_expr: Expression,
        output_schema: SchemaRef,
    },
    Policy {
        policy_ref: String,
        on_violation: ViolationAction,
    },
    Finalizer {
        tool_ref: String,
        run_on: FinalizerTrigger, // success | failure | always
    },
    // Protocol nodes
    Agent {
        agent_ref: String,
        input_mapping: ExpressionMap,
        output_schema: SchemaRef,
    },
    McpTool {
        server: String,
        tool: String,
        input_mapping: ExpressionMap,
        output_schema: SchemaRef,
    },
    A2aTask {
        remote_agent: String,
        skill: String,
        input_mapping: ExpressionMap,
        output_schema: SchemaRef,
        stream: bool,
        on_input_required: Option<NodeId>,
    },
    AgentDiscovery {
        skill: String,
        protocol: Option<String>,
        output_binding: String,
    },
}
```

### 4. Edge Definition

```rust
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub condition: Option<Expression>, // evaluated against current state + last node output
}
```

Conditions are simple expression strings evaluated by the policy/expression engine:
```
"output.priority == 'high'"
"state.confidence < 0.8"
"output.status in ['approved', 'auto_approved']"
```

An unconditional edge (no `condition`) is taken when its source node completes and no conditional edge matches.

### 5. Typed State

Every workflow has a **state schema** — a Pydantic model (Python) or JSON Schema. The state is:
- Immutable at transition boundaries (nodes receive a read-only view)
- Updated via **patch events** emitted by each node
- Stored in the event log (the full updated state snapshot is stored alongside the node_completed event)

```rust
pub struct StateUpdate {
    pub node_id: NodeId,
    pub patch: serde_json::Value, // JSON merge patch (RFC 7396)
}
```

State schema versioning: the IR embeds the schema version. Running executions are pinned to their schema version. Incompatible schema upgrades are rejected at IR validation time.

### 6. Scheduler Loop

```
loop {
    let runnable = query_runnable_nodes(workflow_id);
    for node in runnable {
        write_event(NodeScheduled { node_id });
        enqueue(WorkItem { workflow_id, node_id, queue_type });
    }
    wait_for_event_log_change(); // notification or poll
}
```

A node is **runnable** when:
- All predecessor nodes have `node_completed` events
- The node has no `node_scheduled` or `node_started` event yet (or its last lease has expired)
- The node is not gated by an unresolved condition

### 7. Durable Boundaries

After each of these, state is fully checkpointed before execution continues:
- Node completion
- Interrupt raised (approval, wait)
- External event received
- Retry timer created
- Child workflow completion
- Timer fired

Between durable boundaries, execution may be nondeterministic (model sampling, tool I/O). This is acceptable — nondeterminism is contained to leaf nodes, not orchestration logic.

### 8. Retry Policy

```rust
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: BackoffStrategy,       // Fixed | Exponential | Linear
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub jitter: bool,
    pub retryable_on: Vec<ErrorClass>,  // io_error | timeout | rate_limit | server_error
}
```

On `max_attempts` exhaustion: node moves to `failed`; item sent to dead-letter queue.

### 9. Timeout Hierarchy

```rust
pub struct TimeoutConfig {
    pub node_timeout: Option<Duration>,
    pub workflow_timeout: Option<Duration>,
    pub heartbeat_timeout: Duration,        // default: 30s
    pub approval_timeout: Option<Duration>,
}
```

---

## Drawbacks

- Graph compilation adds latency vs. direct function calls — acceptable for durable systems
- Condition expression language must be sandboxed to prevent injection
- Parallel/join semantics add complexity in the scheduler (barrier synchronization)

---

## Alternatives Considered

### Pure function pipeline (library-style chaining)
Rejected: insufficient for durable execution, branching, parallelism, and HITL.

### Actor model (each node is an actor)
Rejected in v1: more complexity than needed; may revisit for v2 scale requirements.

### Temporal-style workflow functions with replay
Considered: powerful but requires strict determinism in workflow code; our graph model is more transparent and debuggable.

---

## Unresolved Questions

- Expression language for conditions: JSONPath? CEL? Custom DSL? → Lean toward CEL (Common Expression Language) for safety and expressiveness.
- Exactly-once side effects: deferred to v2 via idempotency key pattern.
- Maximum node count per workflow: no hard limit in v1; monitor in production.

---

## Implementation Plan

See progress-tracker.md Workstream A (Phase 1):
- A.1: `jamjet-core` — execution states, node types, retry/timeout models
- A.2: `jamjet-ir` — IR structs, validation
- A.4: `jamjet-scheduler` — scheduling loop
- A.5: `jamjet-worker` — worker execution
