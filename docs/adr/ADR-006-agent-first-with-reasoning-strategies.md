# ADR-006 — Agent-first authoring with pluggable reasoning strategies

| Field | Value |
|---|---|
| ADR | 006 |
| Status | Accepted |
| Date | 2026-03-07 |
| Deciders | JamJet Core Team |
| Supersedes | — |
| Related | RFC-009, ADR-002 (service-first architecture) |

---

## Context

JamJet's initial design is DAG-first: developers define explicit nodes, edges, and conditions. This is the right execution substrate — durable, auditable, predictable. But it is not the right *default authoring experience* for the majority of agent use cases.

Most developers using agent frameworks want to express: "here is my agent, here are its tools, here is the goal, make it happen well." They don't want to design a graph. They discover they need a graph (for compliance, multi-step pipelines, HITL) after they understand the problem — not before.

Several competing frameworks are agent-first by default. Others are DAG-first. JamJet is currently DAG-first.

The question evaluated: **should JamJet remain DAG-first or switch to agent-first with DAG as a power-user mode?**

---

## Decision

**JamJet adopts an agent-first authoring model with pluggable reasoning strategies, while keeping the DAG as the execution substrate and as an explicit authoring option.**

Concretely:

1. **Default authoring**: declare an agent with a strategy, tools, goal, and limits. No graph design required.
2. **Available strategies**: `react`, `plan-and-execute` (default), `critic`, `reflection`, `consensus`, `debate` (see RFC-009).
3. **Every strategy compiles to an IR DAG**. The runtime only executes IR DAGs. Strategies are a compiler-level concept, not a runtime-level one.
4. **`strategy: dag`** is available for any workflow that needs explicit node/edge control. It is the power-user and enterprise path.
5. **Agent nodes inside a DAG can themselves have strategies**. The two models compose.

---

## Rationale

### Why agent-first as default

The adoption funnel argument: developers who are evaluating JamJet against competing frameworks will not be won over by "first, design your graph." They will be won over by "three commands and your agent is running with plan-and-execute reasoning, crash recovery, and full observability."

The graduation argument: developers who write `strategy: plan-and-execute` and then need HITL approval, compliance audit trails, or deterministic routing will naturally reach for `strategy: dag`. The DAG authoring experience is the graduation path, not the entry path.

### Why DAG remains available (not removed)

1. **Enterprise and compliance use cases require DAG.** Auditors and compliance teams need to know exactly what paths can execute. An explicit graph is a compliance artifact; a strategy is not.
2. **Complex multi-agent pipelines need DAG.** When the workflow involves different agents with different responsibilities at specific steps, the graph expresses the intent clearly.
3. **Strategies compile to DAGs** — so there is no conceptual inconsistency. DAG is always what runs.

### Why strategies over raw agent loops

Raw agent loops (ReAct without bounds) are the root cause of the problems that make agents unreliable in production:
- Cost overruns from loop continuation
- Hallucinated "done" signals
- No quality verification step
- Non-deterministic behavior under model updates

Strategies solve these by:
- Making the reasoning pattern explicit and versioned
- Enforcing hard limits (`max_iterations`, `max_cost_usd`, `timeout`) as required config
- Separating the verifier from the executor (critic, plan-and-execute verifier)
- Checkpointing every step — crash recovery works inside a strategy loop

### Why `plan-and-execute` as the default strategy

It outperforms raw ReAct on quality-sensitive tasks (multiple research papers, including Wei et al. 2022, Shinn et al. 2023). It is better suited to multi-step tasks — which is the majority of real agent use cases. It produces an inspectable plan (stored in state) that aids debugging. It has a natural built-in quality gate (the verifier step).

---

## Consequences

### Positive

- First-time developer experience: one YAML file, strategy declaration, no graph design — full durable agent running with plan-and-execute reasoning.
- JamJet is now differentiable from all major competitors on user-facing authoring model *and* on execution quality (strategies + durability vs. raw loops in competing frameworks).
- Strategies are extensible — custom strategies can be added in Phase 4.
- The DAG is not removed — enterprise users are not penalised.

### Negative / risks

- **Implementation complexity**: the strategy→IR compiler adds a new compilation stage. If the IR expansion is buggy, it affects all strategy-based workflows. Mitigation: extensive tests on compiled IR, not just runtime behaviour.
- **Abstraction hiding**: developers using `plan-and-execute` don't see the graph; they may be surprised by the node structure in traces. Mitigation: `jamjet validate --output` shows the compiled IR; docs explain the expansion.
- **Strategy versioning**: if a built-in strategy is improved in a future JamJet release, re-running a historical execution may yield different behaviour. Mitigation: strategies are versioned and running executions pin the strategy version in IR metadata.
- **Non-determinism in planning**: `plan-and-execute` generates a plan via LLM, which is non-deterministic. Replay of a completed execution is deterministic (events are replayed, not regenerated); but a fresh re-run with the same input may generate a different plan. This is documented behaviour.

---

## Alternatives considered

### Alternative 1: Remain DAG-first, add agent node with bound loop
Keep the DAG model as the only authoring approach. Improve the `agent` node type to support configurable reasoning patterns internally.

**Rejected because**: the authoring friction remains — users must still define a DAG even for a single agent. The `agent` node with a `strategy` property is effectively this RFC's approach, but without the top-level `strategy` shorthand. That shorthand is the difference between DAG-first and agent-first.

### Alternative 2: Agent-first only, remove DAG
Make everything an agent with a strategy. Deprecate explicit DAG authoring.

**Rejected because**: DAG is essential for compliance, auditable pipelines, and complex multi-agent orchestration. Removing it would block enterprise adoption.

### Alternative 3: Adopt a competing framework's execution model internally
Use an existing framework's graph model internally and build JamJet's durability layer on top.

**Rejected because**: JamJet's core value is the Rust runtime performance and protocol-native design (MCP/A2A first-class). Adopting another framework's model would require Python-native execution, losing the Rust core, and would inherit that framework's limitations (no protocol adapters, no reasoning strategies, no structured streaming).
