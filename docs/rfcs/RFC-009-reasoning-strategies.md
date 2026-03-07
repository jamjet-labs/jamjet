# RFC-009 — Pluggable Reasoning Strategies

| Field | Value |
|---|---|
| RFC | 009 |
| Title | Pluggable Reasoning Strategies |
| Status | Draft |
| Author | JamJet Core Team |
| Created | 2026-03-07 |
| Depends on | RFC-001 (execution model), RFC-002 (IR schema), RFC-005 (agent model) |

---

## Summary

Introduce a **reasoning strategy** abstraction as a first-class concept in JamJet. A strategy is a named, pre-built reasoning pattern that controls how an agent thinks — plan-then-execute, critic loop, reflection, debate, consensus — compiled internally into a DAG subgraph and executed with full durability, checkpointing, and observability. The DAG remains the execution substrate; strategies are user-friendly configurations over it.

This makes the default authoring experience agent-first (pick a strategy, give it tools and a goal) while preserving the full power of explicit DAG authoring for compliance, enterprise, and complex multi-agent pipelines.

---

## Motivation

### The problem with raw DAG as the default

JamJet's DAG model is powerful but requires the user to design the graph upfront. For the majority of agent use cases — research, analysis, code generation, content creation — the user doesn't know the exact steps in advance. They know the goal, the tools, and the quality bar. Forcing them to design a graph is unnecessary friction.

### The problem with unconstrained agent loops

Pure agent loop frameworks give the LLM full control over execution flow. This is flexible but suffers from:
- Unbounded iteration and cost
- Non-deterministic behavior, hard to reproduce failures
- No audit trail of *why* the model chose each step
- No built-in quality verification before completion
- Poor production safety — an agent in a loop can run indefinitely

### The synthesis

Reasoning strategies are pre-built DAG patterns that capture the best known multi-step reasoning approaches from the research literature. They give developers the simplicity of "pick a strategy" while JamJet provides the underlying durability, checkpointing, cost bounds, and observability that raw agent loops lack.

---

## Design

### 1. Strategy as a first-class workflow concept

A workflow can declare a top-level strategy instead of explicitly defining nodes:

```yaml
# Simple form — single agent with strategy
name: code-reviewer
agent:
  model: claude-3-5-sonnet
  strategy: critic
  tools: [read_file, search_code, run_tests]
  goal: "Review the PR at {{ state.pr_url }} for correctness, security, and performance"
  verifier:
    model: gpt-4o-mini
    criteria: "Did the review address: correctness, security, performance, test coverage?"
  limits:
    max_iterations: 8
    max_cost_usd: 0.50
    timeout: 5m
```

```yaml
# Multi-phase — different strategies per phase
name: research-and-write
strategy: dag   # explicit DAG mode — drop back to full control
nodes:
  research:
    type: agent
    strategy: plan-and-execute
    agent: researcher
    goal: "Research {{ state.topic }} and collect 5+ primary sources"
    limits:
      max_iterations: 12

  draft:
    type: agent
    strategy: critic
    agent: writer
    goal: "Write a 500-word summary based on the research"
    verifier:
      model: gpt-4o-mini
      criteria: "Is the summary accurate, well-structured, and properly cited?"

  approve:
    type: hitl
    prompt: "Review the draft. Approve or request changes."
    next: end
```

### 2. Built-in strategies

#### `react` (default for simple agents)
The foundational ReAct loop — Reason, Act, Observe. Simple, well-understood, appropriate for single-step or exploratory tasks.

```
┌──────────┐    ┌──────────┐    ┌──────────┐
│  Think   │───▶│   Act    │───▶│ Observe  │──┐
│(reasoning│    │(tool call│    │(read     │  │
│  trace)  │    │ or done) │    │ result)  │  │
└──────────┘    └──────────┘    └──────────┘  │
      ▲                                        │
      └────────────────────────────────────────┘
                          │
                    [done signal]
                          │
                       ┌──▼──┐
                       │ End │
                       └─────┘
```

Internal DAG: `think → act → [done? → end] → observe → think`

Limits enforced: `max_iterations`, `max_cost_usd`, `timeout`

---

#### `plan-and-execute` (recommended default)
Generate a structured plan, execute each step, verify against the original goal, replan if needed. Best general-purpose strategy — better quality than ReAct, bounded cost.

```
┌────────┐    ┌─────────────────┐    ┌──────────┐    ┌──────────────┐
│  Plan  │───▶│ Execute Step N  │───▶│  Verify  │───▶│ Goal met?    │
│(LLM gen│    │(tool call or    │    │(separate │    │              │
│ steps) │    │ model call)     │    │ model)   │    │ yes → end    │
└────────┘    └─────────────────┘    └──────────┘    │ no  → replan │
                      ▲                               └──────┬───────┘
                      │                                      │
                 [more steps?]                          ┌────▼──────┐
                      │                                │  Revise   │
                      └──────────────────────────────▶│   Plan    │
                                                       └───────────┘
```

Internal DAG: `plan → execute_step → [more_steps? → execute_step] → verify → [goal_met? → end | replan → plan]`

Key properties:
- Plan is stored in state — inspectable, auditable
- Verifier is a separate model call (not the agent grading itself)
- `max_replans` prevents infinite plan-revise cycles
- Each step execution is a durable boundary (checkpointed)

YAML:
```yaml
strategy: plan-and-execute
verifier:
  model: gpt-4o-mini
  criteria: "Were all required steps completed successfully?"
limits:
  max_iterations: 20
  max_replans: 3
  max_cost_usd: 1.00
```

---

#### `critic`
Execute → Critic evaluates → Revise if poor quality → Re-execute. Best for content generation, code writing, structured output quality.

```
┌─────────┐    ┌──────────┐    ┌──────────────┐
│ Execute │───▶│  Critic  │───▶│ Quality OK?  │
│         │    │(separate │    │              │
│         │    │ model)   │    │ yes → end    │
└─────────┘    └──────────┘    │ no  → revise │
      ▲                        └──────┬───────┘
      │                               │
 [retry]◀──────────────────────────────┘
```

The critic is always a separate model/prompt from the executor — prevents the model from agreeing with itself.

YAML:
```yaml
strategy: critic
verifier:
  model: gpt-4o-mini
  prompt: prompts/code_review_critic.md
  pass_threshold: 0.85   # 0.0–1.0 quality score
limits:
  max_revisions: 3
```

---

#### `reflection`
Single agent reflects on its own output and improves it. Faster and cheaper than critic (one model instead of two) but less objective. Good for lightweight quality improvement on low-stakes tasks.

YAML:
```yaml
strategy: reflection
limits:
  max_reflections: 2
```

---

#### `consensus` (Phase 3)
Multiple independent agents attempt the task; a judge selects the best result. Best for high-stakes single decisions where getting it right matters more than cost.

```
┌──────────┐
│ Agent 1  │──┐
└──────────┘  │   ┌───────┐
┌──────────┐  ├──▶│ Judge │──▶ best result
│ Agent 2  │──┤   └───────┘
└──────────┘  │
┌──────────┐  │
│ Agent 3  │──┘
└──────────┘
```

YAML:
```yaml
strategy: consensus
agents: 3
judge:
  model: gpt-4o
  criteria: "Which answer is most accurate, complete, and well-reasoned?"
```

---

#### `debate` (Phase 3)
A proposer argues for an answer; a critic argues against; a judge decides. Surfaces weaknesses in reasoning. Good for decisions under uncertainty.

```
┌──────────┐         ┌──────────┐
│ Proposer │──────── │  Critic  │
│ (for)    │◀ debate ▶ (against)│
└────┬─────┘         └────┬─────┘
     │                    │
     └─────────┬──────────┘
               │
           ┌───▼───┐
           │ Judge │──▶ decision
           └───────┘
```

YAML:
```yaml
strategy: debate
rounds: 2
judge:
  model: gpt-4o
```

---

#### `dag` (power user / enterprise)
Explicit author-defined DAG. Full control. Agent nodes inside the DAG *may themselves use strategies*. Same as the current JamJet v1 model.

```yaml
strategy: dag
nodes:
  ...   # explicit node definitions
```

---

### 3. IR compilation

Each strategy compiles to a fully explicit IR DAG. The compiler expands `strategy: plan-and-execute` into the equivalent explicit node/edge graph before handing off to the runtime. The runtime knows nothing about strategies — it only executes IRs.

This means:
- Strategies inherit all durability properties (checkpointing at every node boundary)
- Strategies appear in execution traces as their actual node steps
- Strategies can be inspected, replayed, and debugged like any workflow
- Power users can request the compiled IR: `jamjet validate --output workflow.yaml`

### 4. Limits enforcement

All strategies require explicit limits. Defaults are conservative:

| Limit | Default | Notes |
|---|---|---|
| `max_iterations` | 10 | Maximum ReAct or step iterations |
| `max_replans` | 2 | Maximum plan revision cycles (plan-and-execute) |
| `max_revisions` | 2 | Maximum critic revision cycles |
| `max_cost_usd` | None (required) | Hard cost cap — execution halts if exceeded |
| `timeout` | 10m | Wall-clock timeout per strategy execution |

If `max_cost_usd` is omitted in production mode, the runtime emits a warning. In strict mode (`JAMJET_STRICT=true`), it is an error.

### 5. Observability

Each strategy step produces standard JamJet events:
- `strategy_started` — includes strategy name, model, goal
- `plan_generated` — for plan-and-execute, stores the plan in state (inspectable)
- `step_started` / `step_completed` — per execution step
- `critic_verdict` — includes score, pass/fail, feedback text
- `revision_started` — when quality loop triggers a revision
- `strategy_completed` — includes final quality score, iteration count, cost

The `jamjet inspect` command shows a strategy-aware view:
```
Strategy: plan-and-execute    Iterations: 4/20    Cost: $0.12    Status: ✓ complete

Plan:
  ✓ Step 1 — Search arxiv for papers on transformer attention
  ✓ Step 2 — Extract key findings from top 3 papers
  ✓ Step 3 — Cross-reference with recent benchmarks
  ✓ Step 4 — Synthesize into structured report

Verifier: PASS (score: 0.91)  "All steps completed, sources cited, claims verified"
```

---

## Backwards compatibility

This RFC is purely additive:
- Existing explicit DAG workflows are unchanged. `strategy: dag` is the existing model.
- The `strategy` key at workflow or node level is new and optional.
- All existing node types remain valid inside `strategy: dag` workflows.
- IR schema is extended (RFC-002 update required) to carry `strategy_metadata` on compiled agent nodes.

---

## Implementation plan

| Phase | Scope |
|---|---|
| Phase 2 | `react` strategy — baseline, replaces raw agent loop node. Implements strategy→IR compiler. Limits enforcement. |
| Phase 2 | `plan-and-execute` strategy — general-purpose default. Verifier model support. `max_replans`. |
| Phase 2 | `critic` strategy — quality loops. Separate verifier prompt. `pass_threshold`. |
| Phase 3 | `reflection` strategy — lightweight single-model improvement. |
| Phase 3 | `consensus` strategy — multi-agent voting + judge. |
| Phase 3 | `debate` strategy — proposer/critic/judge reasoning. |
| Phase 3 | Strategy observability (`plan_generated`, `critic_verdict` events, `jamjet inspect` strategy view). |
| Phase 3 | `jamjet validate --output` shows compiled IR for any strategy. |

---

## Open questions

1. **Custom strategies** — should users be able to define custom strategies as named YAML/Python templates? Likely yes, Phase 4.
2. **Nested strategies** — can a `plan-and-execute` agent's execution steps themselves use `critic`? Likely yes, within limits.
3. **Strategy versioning** — if the built-in `plan-and-execute` implementation changes, running executions should pin the strategy version. Needs IR extension.
4. **Verifier model cost attribution** — verifier model calls should be attributed to the parent execution's cost budget. Needs cost accounting extension.
