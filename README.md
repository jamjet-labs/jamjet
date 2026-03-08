<div align="center">

<!-- Lightning bolt logo — ⚡ fallback since GitHub strips inline SVG -->
<h1>⚡ JamJet</h1>

**The agent-native runtime — durable, composable, built for production.**

[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square)](https://rustup.rs)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Java](https://img.shields.io/badge/java-21%2B-red?style=flat-square)](https://openjdk.org)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev/docs/quickstart)

[jamjet.dev](https://jamjet.dev) · [Quickstart](https://jamjet.dev/docs/quickstart) · [Docs](https://jamjet.dev/docs/concepts) · [Examples](https://jamjet.dev/examples) · [Blog](https://jamjet.dev/blog)

</div>

<div align="center">

![JamJet demo](https://raw.githubusercontent.com/jamjet-labs/jamjet/main/demo.gif)

</div>

---

JamJet is a **performance-first, agent-native runtime** for AI agents. It is not another prompt wrapper or thin agent SDK — it is a **production-grade orchestration substrate** for agents that need to work, not just demo.

The runtime core is **Rust + Tokio** for scheduling, state, and concurrency. The authoring surface is **Python**, **Java**, or **YAML**. All compile to the same IR graph and run on the same engine.

## Why JamJet?

| Problem | JamJet's answer |
|---------|----------------|
| Agent runs lose state on crash | **Durable graph execution** — event-sourced, crash-safe resume |
| No way to pause for human approval | **Human-in-the-loop** as a first-class workflow primitive |
| Agents siloed in their own framework | **Native MCP + A2A** — interoperate with any agent, any framework |
| Slow Python orchestration at scale | **Rust core** — no GIL, real async parallelism |
| Weak observability, no replay | **Full event timeline**, OTel GenAI traces, replay from any checkpoint |
| No standard agent identity | **Agent Cards** — every agent is addressable and discoverable |
| No governance or guardrails | **Policy engine** — tool blocking, approvals, autonomy enforcement, audit log |
| Locked into one language | **Polyglot SDKs** — Python, Java (JDK 21), YAML — same IR, same runtime |
| Can't run without a server | **In-process execution** — `pip install jamjet` and run immediately |

---

## Quickstart

**Requirements:** Python 3.11+

### Fastest path — pure Python, no server

```bash
pip install jamjet
```

```python
from jamjet import task, tool

@tool
async def web_search(query: str) -> str:
    return f"Search results for: {query}"

@task(model="claude-haiku-4-5-20251001", tools=[web_search])
async def research(question: str) -> str:
    """You are a research assistant. Search first, then summarize clearly."""

result = await research("What is JamJet?")
print(result)
```

No server. No config. No YAML. Just `pip install` and run.

### Full runtime path — durable execution

```bash
pip install jamjet
jamjet init my-first-agent
cd my-first-agent
jamjet dev
```

In another terminal:

```bash
jamjet run workflow.yaml --input '{"query": "What is JamJet?"}'
```

→ **[Full quickstart guide](https://jamjet.dev/docs/quickstart)**

---

## Hello World

### YAML

```yaml
# workflow.yaml
workflow:
  id: hello-agent
  version: 0.1.0
  state_schema:
    query: str
    answer: str
  start: think

nodes:
  think:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: "Answer clearly and concisely: {{ state.query }}"
    output_key: answer
    next: end

  end:
    type: end
```

```bash
jamjet validate workflow.yaml
jamjet run workflow.yaml --input '{"query": "What is JamJet?"}'
```

### Python — `@task` (simplest)

```python
from jamjet import task, tool

@tool
async def web_search(query: str) -> str:
    return f"Search results for: {query}"

@task(model="claude-haiku-4-5-20251001", tools=[web_search])
async def research(question: str) -> str:
    """You are a research assistant. Search first, then summarize clearly."""

result = await research("What is JamJet?")
```

The docstring becomes the instruction. The function signature is the contract. That's it.

### Python — `Agent`

```python
from jamjet import Agent, tool

@tool
async def web_search(query: str) -> str:
    return f"Search results for: {query}"

agent = Agent(
    "researcher",
    model="claude-haiku-4-5-20251001",
    tools=[web_search],
    instructions="You are a research assistant. Search first, then summarize.",
)

result = await agent.run("What is JamJet?")
print(result)
```

### Python — `Workflow` (full control)

```python
from jamjet import Workflow, tool
from pydantic import BaseModel

@tool
async def web_search(query: str) -> str:
    return f"Search results for: {query}"

workflow = Workflow("research")

@workflow.state
class State(BaseModel):
    query: str
    answer: str | None = None

@workflow.step
async def search(state: State) -> State:
    result = await web_search(query=state.query)
    return state.model_copy(update={"answer": result})
```

All three levels compile to the same IR and run on the same durable Rust runtime.

### Performance

JamJet's IR compilation is **88× faster** than LangGraph's graph compilation:

| Operation | JamJet | LangGraph |
|-----------|--------|-----------|
| Compile / graph build | **~0.006 ms** | ~0.529 ms |
| In-process invocation | **~0.015 ms** | ~1.458 ms |

Measured with Python 3.11, single-tool workflows. JamJet compiles a lightweight IR dict; LangGraph builds a NetworkX graph.

### MCP tool call

```yaml
nodes:
  search:
    type: tool
    server: brave-search        # configured in jamjet.toml
    tool: web_search
    arguments:
      query: "{{ state.query }}"
      count: 10
    output_key: results
    next: summarize
```

### A2A delegation

```yaml
nodes:
  delegate:
    type: a2a_task
    agent_url: "https://agents.example.com/research-agent"
    input:
      query: "{{ state.query }}"
    output_key: research
    next: end
```

### Eval with self-improvement

```yaml
nodes:
  check:
    type: eval
    scorers:
      - type: llm_judge
        rubric: "Is the answer accurate and complete?"
        min_score: 4
    on_fail: retry_with_feedback   # injects feedback into next model call
    max_retries: 2
    next: end
```

---

## How JamJet compares

> As of March 2026. All frameworks evolve — check their docs for the latest.

| Capability | JamJet | LangChain | AutoGen | CrewAI |
|------------|--------|-----------|---------|--------|
| **Simple agent setup** | ✅ 3 lines (`@task`) | 6+ lines | 10+ lines | 8+ lines |
| **In-process execution** | ✅ `pip install` + run | ✅ native | ✅ native | ✅ native |
| **Durable execution** | ✅ event-sourced, crash-safe | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral |
| **Human-in-the-loop** | ✅ first-class primitive | 🟡 callbacks | 🟡 conversational | 🟡 manual |
| **MCP support** | ✅ client + server | 🟡 client only | 🟡 client only | 🟡 client only |
| **A2A protocol** | ✅ client + server | ❌ | ❌ | ❌ |
| **Built-in eval** | ✅ LLM judge, assertions | ❌ | ❌ | ❌ |
| **Built-in observability** | ✅ OTel GenAI, event replay | 🟡 LangSmith (external) | ❌ | ❌ |
| **Agent identity** | ✅ Agent Cards, A2A discovery | ❌ | ❌ | ❌ |
| **Policy & governance** | ✅ policy engine, audit log | ❌ | ❌ | ❌ |
| **Progressive complexity** | ✅ `@task` → `Agent` → `Workflow` | ❌ single API | ❌ | ❌ |
| **Runtime language** | Rust core + Python/Java authoring | Python | Python | Python |
| **Best for** | Production multi-agent systems | Rapid prototyping | Conversational agents | Role-based crews |

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     Authoring Layer                       │
│         Python SDK  |  Java SDK  |  YAML  |  CLI          │
├──────────────────────────────────────────────────────────┤
│                 Compilation / Validation                   │
│           Graph IR  |  Schema  |  Policy lint             │
├────────────────────────────┬─────────────────────────────┤
│      Rust Runtime Core     │      Protocol Layer          │
│  Scheduler  |  State SM    │  MCP Client  |  MCP Server   │
│  Event log  |  Snapshots   │  A2A Client  |  A2A Server   │
│  Workers    |  Timers      │                              │
├────────────────────────────┴─────────────────────────────┤
│                    Enterprise Services                     │
│  Policy Engine  |  Audit Log  |  Autonomy Enforcement     │
├──────────────────────────────────────────────────────────┤
│                      Runtime Services                      │
│  Model Adapters  |  Tool Execution  |  Observability      │
├──────────────────────────────────────────────────────────┤
│                         Storage                           │
│           Postgres (production)  |  SQLite (local)        │
└──────────────────────────────────────────────────────────┘
```

## Roadmap

| Phase | Status | Goal |
|-------|--------|------|
| 0 — Architecture & RFCs | ✅ Complete | Design docs, RFCs, repo scaffolding |
| 1 — Minimal Viable Runtime | ✅ Complete | Local durable execution, MCP client, agent cards, Python CLI |
| 2 — Production Core | ✅ Complete | Distributed workers, MCP server, full A2A client + server |
| 3 — Developer Delight | ✅ Complete | Eval harness, trace debugging, templates, Java SDK |
| 4 — Enterprise | 🔄 In Progress | Policy engine, autonomy enforcement, audit log, budgets |
| 5 — Scale & Ecosystem | 📋 Planned | TypeScript SDK, hosted plane, agent marketplace |

---

## Documentation

Full documentation at **[jamjet.dev/docs](https://jamjet.dev/docs/quickstart)**

| | |
|--|--|
| [Quickstart](https://jamjet.dev/docs/quickstart) | Get running in 10 minutes |
| [Core Concepts](https://jamjet.dev/docs/concepts) | Agents, workflows, nodes, state, durability |
| [YAML Workflows](https://jamjet.dev/docs/yaml-workflows) | All node types, retry policies, conditions |
| [Python SDK](https://jamjet.dev/docs/python-sdk) | Full Python SDK reference |
| [Java SDK](https://jamjet.dev/docs/java-sdk) | JDK 21, virtual threads, records |
| [MCP Integration](https://jamjet.dev/docs/mcp) | Connect to MCP servers, expose tools |
| [A2A Integration](https://jamjet.dev/docs/a2a) | Delegate to and serve external agents |
| [Eval Harness](https://jamjet.dev/docs/eval) | Score quality, run regression suites, gate CI |
| [Observability](https://jamjet.dev/docs/observability) | OTel traces, metrics, Prometheus |
| [Deployment](https://jamjet.dev/docs/deployment) | Docker, Kubernetes, PostgreSQL |
| [CLI Reference](https://jamjet.dev/docs/cli) | Full CLI reference |

---

## Repository structure

```
jamjet/
├── runtime/                # Rust workspace (15 crates)
│   ├── core/               # Graph IR, node types, state machine
│   ├── ir/                 # Canonical Intermediate Representation
│   ├── scheduler/          # Durable task scheduler
│   ├── state/              # Event-sourced state, snapshots
│   ├── workers/            # Node executors (model, tool, http, eval, …)
│   ├── api/                # REST API control plane
│   ├── agents/             # Agent Cards, registry, lifecycle
│   ├── models/             # LLM provider adapter layer
│   ├── timers/             # Durable timers, Postgres-backed cron
│   ├── policy/             # Policy engine (tool blocking, approvals)
│   ├── audit/              # Immutable audit log
│   ├── protocols/
│   │   ├── mcp/            # MCP client + server
│   │   └── a2a/            # A2A client + server
│   └── telemetry/          # OTel instrumentation
├── sdk/
│   ├── python/             # Python SDK + CLI
│   │   └── jamjet/
│   │       ├── cli/        # jamjet CLI (Typer)
│   │       ├── eval/       # Eval dataset, runner, scorers
│   │       ├── agents/     # Agent definitions + strategies
│   │       ├── templates/  # Project scaffolding templates
│   │       └── workflow/   # Python workflow builder
│   └── java/               # Java SDK (JDK 21, virtual threads, records)
│       ├── jamjet-sdk/     # Core SDK module
│       └── jamjet-cli/     # CLI module
```

---

## Contributing

Contributions are welcome — bugs, features, docs, and code.

- Open an issue for bugs or feature requests
- Check issues tagged `good first issue` for easy entry points
- For large changes, open an issue first to discuss the approach
- Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup instructions

---

## Community

- **GitHub Discussions:** [github.com/jamjet-labs/jamjet/discussions](https://github.com/jamjet-labs/jamjet/discussions)
- **GitHub Issues:** [github.com/jamjet-labs/jamjet/issues](https://github.com/jamjet-labs/jamjet/issues)

---

## License

Apache 2.0 — see [LICENSE](LICENSE).

---

<div align="center">
  <sub>Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet · <a href="https://jamjet.dev">jamjet.dev</a> · Apache 2.0</sub>
</div>
