# JamJet

> **The agent-native runtime — built for performance, designed for interoperability, reliable enough for production.**

[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](https://github.com/jamjet-labs/jamjet/blob/main/LICENSE)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev)

![JamJet demo](https://raw.githubusercontent.com/jamjet-labs/jamjet/main/demo.gif)

JamJet is a **performance-first, agent-native runtime and framework** for building reliable, interoperable AI agent systems. It is not another prompt wrapper or thin agent SDK — it is an **orchestration substrate** built to prevent the failure modes that hurt real agents: lost state on crash, skipped approvals, runaway cost, and siloed observability.

---

## Why JamJet?

| Problem | JamJet's answer |
|---------|----------------|
| Fragile agent runs that lose state on crash | **Durable graph execution** with event sourcing and crash recovery |
| No way to pause for human approval | **Human-in-the-loop** as a first-class workflow primitive |
| Agents siloed in their own framework | **Native MCP + A2A** — interoperate with any agent, any framework |
| Slow Python orchestration at scale | **Rust core** for scheduling, state, concurrency; Python for authoring |
| Weak observability, no replay | **Full event timeline**, replay from any checkpoint |
| No standard agent identity | **Agent Cards** — every agent is addressable, discoverable, composable |
| Inconsistent tool contracts | **Typed schemas everywhere** — Pydantic, TypedDict, JSON Schema |

---

## How JamJet compares

> As of March 2026. All frameworks evolve quickly — check their docs for the latest.

| Capability | JamJet | LangChain | AutoGen | CrewAI | BeeAI |
|------------|--------|-----------|---------|--------|-------|
| **Default model** | Agent-first + strategy | Chain / LCEL | Conversational agents | Role-based crew | Agent loop |
| **Durable execution** | ✅ event-sourced; crash-safe resume | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral |
| **Human-in-the-loop** | ✅ first-class primitive | 🟡 callback hooks | 🟡 first-class for conversational flows | 🟡 manual | ❌ |
| **MCP support** | ✅ client + server | 🟡 client only | 🟡 client only | 🟡 client only | 🟡 client only |
| **A2A protocol** | ✅ client + server | ❌ | ❌ | ❌ | ❌ |
| **Pluggable reasoning strategies** | ✅ react, plan-and-execute, critic, reflection, consensus, debate | ❌ manual wiring | 🟡 user-built | 🟡 role-based tasks | 🟡 user-built |
| **Enforced cost/iteration limits** | ✅ compile-time guards | ❌ | 🟡 partial | 🟡 partial | ❌ |
| **Typed state schemas** | ✅ Pydantic / TypedDict / JSON Schema | 🟡 optional | 🟡 optional | 🟡 partial | 🟡 partial |
| **Built-in observability** | ✅ OTel GenAI, full event timeline, replay | 🟡 LangSmith (external) | ❌ | ❌ | ❌ |
| **Agent identity + discovery** | ✅ Agent Cards, A2A discovery | ❌ | ❌ | ❌ | 🟡 framework-level discovery |
| **Runtime language** | Rust core + Python authoring | Python | Python | Python | TypeScript |
| **Scheduler / throughput** | ✅ Rust async scheduler; low-overhead worker pool | 🟡 Python asyncio; GIL-bound | 🟡 Python asyncio; message-passing overhead | 🟡 Python; sequential by default | 🟡 Node.js event loop |
| **Deployment model** | Long-running server (local or cloud) | Library (in-process) | Library (in-process) | Library (in-process) | Library (in-process) |
| **Best for** | Production multi-agent systems needing durability and interop | Rapid prototyping, LLM chains | Conversational multi-agent apps | Role-based collab agents | TypeScript agentic apps |

---

## Key Features

**Durable graph workflows** — every step is checkpointed; crash the runtime and execution resumes exactly where it left off.

**Native MCP + A2A** — connect to any MCP server, expose your tools as an MCP server, delegate to or serve external agents via A2A.

**Agent-native identity** — every agent has a URI, an Agent Card describing its capabilities, and a managed lifecycle.

**Human-in-the-loop** — pause any workflow for human approval or input as a first-class primitive.

**Configurable autonomy** — from strict deterministic graph execution to bounded autonomous agents operating within defined budgets.

**Typed schemas everywhere** — Pydantic, TypedDict, JSON Schema. No stringly-typed state.

**Distributed workers** — horizontal scale with lease semantics, retry policies, and queue isolation by workload type.

**Python-friendly authoring** — define workflows with Python decorators or YAML; both compile to the same runtime graph.

---

## Quick Start

**Requirements:** Python 3.11+

```bash
# Install the CLI (runtime binary included)
pip install jamjet

# Option A — scaffold a new project
jamjet init my-agent-project
cd my-agent-project

# Option B — add JamJet to an existing project (works like git init)
cd my-existing-project
jamjet init

# Start local dev runtime (SQLite, embedded — no server setup needed)
jamjet dev
```

In another terminal:

```bash
# Run the example workflow
jamjet run examples/basic_tool_flow
```

### Hello World — Python

```python
from jamjet import Workflow, tool
from pydantic import BaseModel

class SearchResult(BaseModel):
    summary: str
    sources: list[str]

@tool
async def web_search(query: str) -> SearchResult:
    # your implementation here
    ...

workflow = Workflow("research")

@workflow.state
class ResearchState(BaseModel):
    question: str
    result: SearchResult | None = None

@workflow.step
async def search(state: ResearchState) -> ResearchState:
    result = await web_search(query=state.question)
    return state.model_copy(update={"result": result})

@workflow.step
async def summarize(state: ResearchState) -> ResearchState:
    # model call here
    ...
```

### Hello World — YAML

```yaml
# workflow.yaml
workflow:
  id: research
  version: 0.1.0
  state_schema: schemas.ResearchState
  start: search

nodes:
  search:
    type: tool
    tool_ref: tools.web_search
    input:
      query: "{{ state.question }}"
    output_schema: schemas.SearchResult
    next: summarize

  summarize:
    type: model
    model: default_chat
    prompt: prompts/summarize.md
    output_schema: schemas.Summary
    next: end
```

```bash
jamjet validate workflow.yaml
jamjet run workflow.yaml --input '{"question": "What is JamJet?"}'
```

### Connecting to an MCP Server

```yaml
# agents.yaml
agents:
  researcher:
    model: default_chat
    mcp:
      servers:
        github:
          transport: http_sse
          url: https://mcp.github.com/v1
          auth:
            type: bearer
            token_env: GITHUB_TOKEN
```

```yaml
# workflow.yaml (node using MCP tool)
nodes:
  search_github:
    type: mcp_tool
    server: github
    tool: search_code
    input:
      query: "{{ state.search_query }}"
    output_schema: schemas.SearchResults
    retry_policy: io_default
```

### Delegating to an External Agent via A2A

```yaml
nodes:
  code_review:
    type: a2a_task
    remote_agent: partner_reviewer   # defined in agents.yaml
    skill: security_review
    input:
      code: "{{ state.generated_code }}"
    stream: true
    timeout: 300s
    on_input_required: human_review
```

---

## Durability across frameworks

Wrap any side-effecting tool with `@durable` to get exactly-once execution
across crashes, restarts, and replays — regardless of which agent framework
you're using.

```python
from jamjet import durable

@durable
def charge_card(amount: float) -> dict:
    return stripe.charges.create(amount=amount)
```

Pair it with a `durable_run()` context manager — one shim per framework:

| Framework | Import |
|---|---|
| LangChain | `from jamjet.langchain import durable_run` |
| CrewAI | `from jamjet.crewai import durable_run` |
| Google ADK | `from jamjet.adk import durable_run` |
| Anthropic Agent SDK | `from jamjet.anthropic_agent import durable_run` |
| OpenAI Agents SDK | `from jamjet.openai_agents import durable_run` |

Example with LangChain:

```python
from langchain.agents import AgentExecutor
from jamjet import durable, durable_run

@durable
def charge_card(amount): ...  # your real tool

executor = AgentExecutor(...)

# Use a stable execution_id that survives process restarts.
# (Persist this id in your job queue / DB / wherever you start agent runs.)
AGENT_RUN_ID = "booking-agent-run-abc123"

with durable_run(AGENT_RUN_ID):
    executor.invoke({"input": "book a flight to Tokyo"})
    # If the process crashes mid-`charge_card` and restarts under the same
    # AGENT_RUN_ID, the cached result is returned on replay — Stripe is
    # never called twice.
```

For framework-native run-identity (where you'd otherwise want `with durable_run(executor):`), see the per-shim docs in `jamjet/<framework>/__init__.py` — note that most frameworks expose `run_id` only per-invocation via callbacks, so threading a stable identity across crash boundaries is the user's responsibility.

See the per-module guide at [`jamjet/durable/README.md`](./jamjet/durable/README.md).

---

## JamJet Cloud — Hosted Governance (`jamjet.cloud`)

Starting in **0.6.0**, the `jamjet` package includes a `jamjet.cloud` submodule for the hosted control plane. Two-line install:

```python
import jamjet.cloud as jamjet
from openai import OpenAI

jamjet.configure(api_key="jj_xxxxxxxxxxxx", project="my-agent")

# Every OpenAI / Anthropic call is now captured automatically.
resp = OpenAI().chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "hello"}],
)
```

Open [app.jamjet.dev/dashboard/traces](https://app.jamjet.dev/dashboard/traces) — the call appears within ~5 seconds with model, tokens, cost, and duration.

Optional governance primitives:

```python
jamjet.policy("block", "payments.*")              # filter tools by name
jamjet.budget(max_cost_usd=5.00)                   # cap spend; raises BudgetExceeded
approval = jamjet.require_approval(action="charge_card", context={...})
@jamjet.trace                                       # decorate any function
def lookup_customer(...): ...
```

Install with the LLM SDK you use:

```bash
pip install jamjet[openai]      # auto-instrument OpenAI
pip install jamjet[anthropic]   # or Anthropic
pip install jamjet[cloud-all]   # both
```

Full guide: [Cloud Quickstart](https://docs.jamjet.dev/en/docs/cloud-quickstart) · [Sign up free](https://app.jamjet.dev)

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     Authoring Layer                       │
│              Python SDK  |  YAML  |  CLI                  │
├──────────────────────────────────────────────────────────┤
│                 Compilation / Validation                   │
│         Graph IR  |  Schema  |  Policy lint               │
├────────────────────────────┬─────────────────────────────┤
│      Rust Runtime Core     │      Protocol Layer          │
│  Scheduler  |  State SM    │  MCP Client  |  MCP Server   │
│  Event log  |  Snapshots   │  A2A Client  |  A2A Server   │
│  Workers    |  Timers      │  Protocol Adapter Framework  │
├────────────────────────────┴─────────────────────────────┤
│                      Runtime Services                      │
│  Model Adapters  |  Tool Execution  |  Memory/Retrieval   │
│  Policy Engine   |  Observability   |  Secret Management  │
├──────────────────────────────────────────────────────────┤
│                    Control Plane / APIs                    │
│        REST / gRPC  |  Agent Registry  |  Admin           │
├──────────────────────────────────────────────────────────┤
│                         Storage                           │
│           Postgres (production)  |  SQLite (local)        │
└──────────────────────────────────────────────────────────┘
```

Read more: [Architecture Overview](docs/architecture/overview.md)

---

## Documentation

### Start here

| Guide | Description |
|-------|-------------|
| [Quickstart](docs/guides/quickstart.md) | Get a workflow running in 10 minutes |
| [Core Concepts](docs/guides/concepts.md) | Agents, workflows, nodes, state, durability |
| [Workflow Authoring](docs/guides/workflow-authoring.md) | YAML and Python authoring guide |
| [Python SDK](docs/guides/python-sdk.md) | Full SDK reference |
| [YAML Reference](docs/guides/yaml-reference.md) | Complete YAML spec |

### Connect to other systems

| Guide | Description |
|-------|-------------|
| [MCP Integration](docs/guides/mcp-guide.md) | Connect to MCP servers, expose tools |
| [A2A Integration](docs/guides/a2a-guide.md) | Delegate to and serve external agents |
| [Agent Model](docs/guides/agent-model-guide.md) | Agent Cards, lifecycle, autonomy levels |
| [Human-in-the-Loop](docs/guides/hitl.md) | Approval nodes, state editing, audit |
| [Observability](docs/guides/observability.md) | Traces, replay, cost attribution, OTel GenAI |
| [Deployment](docs/guides/deployment.md) | Production deployment guide |

### Advanced & enterprise

| Guide | Description |
|-------|-------------|
| [Security](docs/guides/security.md) | Auth, secrets, RBAC, OAuth delegated agent auth |
| [WASI Sandboxing](docs/guides/wasi-sandboxing.md) | Sandboxed tool execution via Wasmtime |
| [Eval Node](docs/guides/eval.md) | Evaluation as a workflow primitive |
| [ANP Discovery](docs/guides/anp.md) | Decentralized agent discovery via DID |

### Architecture deep-dives

| Document | Description |
|----------|-------------|
| [Architecture Overview](docs/architecture/overview.md) | Full system architecture |
| [Execution Model](docs/architecture/execution-model.md) | State machine, node types, IR |
| [State & Durability](docs/architecture/state-and-durability.md) | Event sourcing, snapshotting, recovery |
| [Agent Model](docs/architecture/agent-model.md) | Agent-native runtime design |
| [MCP Architecture](docs/architecture/mcp-integration.md) | MCP client/server internals |
| [A2A Architecture](docs/architecture/a2a-integration.md) | A2A protocol internals |
| [Protocol Adapters](docs/architecture/protocol-adapters.md) | Extensible protocol layer |

---

## Roadmap

| Phase | Status | Goal |
|-------|--------|------|
| 0 — Architecture & RFCs | In progress | Design docs, RFCs, scaffolding |
| 1 — Minimal Viable Runtime | Planned | Local durable execution, MCP client, agent cards |
| 2 — Production Core | Planned | Distributed workers, MCP server, full A2A |
| 3 — Developer Delight | Planned | Templates, eval harness, trace debugging |
| 4 — Enterprise | Planned | Policy engine, tenant isolation, federation security |
| 5 — Scale & Ecosystem | Planned | Go SDK, TypeScript SDK, hosted plane, agent marketplace |

Track milestone progress in [GitHub Issues](https://github.com/jamjet-labs/jamjet/issues) and the project board.

---

## Contributing

We welcome contributions of all kinds — bug reports, feature requests, documentation, and code.

- Read [CONTRIBUTING.md](CONTRIBUTING.md) to get started
- Check open issues for `good first issue` tags
- RFCs for major changes: see [docs/rfcs/](docs/rfcs/)
- Architecture decisions: see [docs/adr/](docs/adr/)

---

## Community

- **GitHub Discussions:** [github.com/jamjet-labs/jamjet/discussions](https://github.com/jamjet-labs/jamjet/discussions)


---

## License

Apache 2.0 — see [LICENSE](LICENSE).
