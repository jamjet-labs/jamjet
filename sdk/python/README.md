# JamJet

> *Write the safety policy once. Run it everywhere your agents can act.*

[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](https://github.com/jamjet-labs/jamjet/blob/main/LICENSE)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev)

![JamJet demo](https://raw.githubusercontent.com/jamjet-labs/jamjet/main/demo.gif)

JamJet sits underneath your Python agent — Claude Code, OpenAI Agents SDK, MCP clients, LangChain, CrewAI, ADK, custom code — and enforces what prompts cannot:

- 🛡️ **Block unsafe tool calls** at runtime (database deletes, payments, file writes)
- ✋ **Pause for human approval** on risky actions, durably
- 💸 **Cap cost** per agent, per run, per project
- 📒 **Record an audit trail** that survives a regulator's review
- ⏪ **Replay or resume** crashed runs from the last checkpoint

**Keep your agent framework. Add JamJet where tool calls need control.**

---

## See it in 60 seconds

```bash
pip install jamjet
jamjet demo unsafe-tool-call
```

No API key. No Docker. No cloud account. The model is mocked; the enforcement path is real. Three more demos run the same way:

```bash
jamjet demo approval        # pause-for-approval flow
jamjet demo budget-cap      # $0.05 cost cap
jamjet demo mcp-tool-policy # MCP-shaped policy
```

The rest of this README is the **Python authoring guide** — workflows, durability shims, MCP/A2A integration, and the hosted control plane. For the full positioning + cross-language adapters (Claude Code hook, MCP shim, OpenAI guardrail, TypeScript SDK, CLI), see the [main repo README](https://github.com/jamjet-labs/jamjet#readme).

---

## What you get in the Python package

- **`jamjet demo <scenario>`** — runnable enforcement demos with no setup
- **Policy as code** — YAML beside your workflow OR `jamjet.cloud.policy(...)` in Python
- **`@durable`** — exactly-once execution across crashes/restarts for any side-effecting tool, with shims for LangChain, CrewAI, ADK, OpenAI Agents SDK, Anthropic Agent SDK
- **`jamjet.cloud`** — optional two-line install for the hosted control plane (free tier: 5K traces/mo, single project)
- **`jamjet.integrations.openai_guardrail`** — drop-in guardrail for the OpenAI Agents SDK that runs the same `policy.yaml`
- **MCP client + A2A** — connect to MCP servers and delegate to external agents from a workflow; the runtime carries trace context across both protocols

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
