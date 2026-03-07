<div align="center">

<!-- Pixel art lightning bolt logo -->
<svg width="80" height="96" viewBox="0 0 30 36" fill="none" xmlns="http://www.w3.org/2000/svg">
  <rect x="10" y="0"  width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="0"  width="5" height="4" fill="#f5c518"/>
  <rect x="5"  y="4"  width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="4"  width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="4"  width="5" height="4" fill="#f5c518"/>
  <rect x="20" y="4"  width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="8"  width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="8"  width="5" height="4" fill="#f5c518"/>
  <rect x="0"  y="12" width="5" height="4" fill="#f5c518"/>
  <rect x="5"  y="12" width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="12" width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="12" width="5" height="4" fill="#f5c518"/>
  <rect x="5"  y="16" width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="16" width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="16" width="5" height="4" fill="#f5c518"/>
  <rect x="20" y="16" width="5" height="4" fill="#f5c518"/>
  <rect x="25" y="16" width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="20" width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="20" width="5" height="4" fill="#f5c518"/>
  <rect x="10" y="24" width="5" height="4" fill="#f5c518"/>
  <rect x="15" y="24" width="5" height="4" fill="#f5c518"/>
</svg>

# JamJet

**The agent-native runtime — durable, composable, built for production.**

[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square)](https://rustup.rs)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev/docs/quickstart)

[jamjet.dev](https://jamjet.dev) · [Quickstart](https://jamjet.dev/docs/quickstart) · [Docs](https://jamjet.dev/docs/concepts) · [Examples](https://jamjet.dev/examples) · [Blog](https://jamjet.dev/blog)

</div>

---

JamJet is a **performance-first, agent-native runtime** for AI agents. It is not another prompt wrapper or thin agent SDK — it is a **production-grade orchestration substrate** for agents that need to work, not just demo.

The runtime core is **Rust + Tokio** for scheduling, state, and concurrency. The authoring surface is **Python** (or YAML). Both compile to the same IR graph and run on the same engine.

## Why JamJet?

| Problem | JamJet's answer |
|---------|----------------|
| Agent runs lose state on crash | **Durable graph execution** — event-sourced, crash-safe resume |
| No way to pause for human approval | **Human-in-the-loop** as a first-class workflow primitive |
| Agents siloed in their own framework | **Native MCP + A2A** — interoperate with any agent, any framework |
| Slow Python orchestration at scale | **Rust core** — no GIL, real async parallelism |
| Weak observability, no replay | **Full event timeline**, OTel GenAI traces, replay from any checkpoint |
| No standard agent identity | **Agent Cards** — every agent is addressable and discoverable |

---

## Quickstart

**Requirements:** Python 3.11+

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

### Python SDK

```python
from jamjet import workflow, node, State

@workflow(id="hello-agent", version="0.1.0")
class HelloAgent:
    @node(start=True)
    async def think(self, state: State) -> State:
        response = await self.model(
            model="claude-haiku-4-5-20251001",
            prompt=f"Answer clearly: {state['query']}",
        )
        return {"answer": response.text}
```

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
| **Durable execution** | ✅ event-sourced, crash-safe | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral |
| **Human-in-the-loop** | ✅ first-class primitive | 🟡 callbacks | 🟡 conversational | 🟡 manual |
| **MCP support** | ✅ client + server | 🟡 client only | 🟡 client only | 🟡 client only |
| **A2A protocol** | ✅ client + server | ❌ | ❌ | ❌ |
| **Built-in eval** | ✅ LLM judge, assertions | ❌ | ❌ | ❌ |
| **Built-in observability** | ✅ OTel GenAI, event replay | 🟡 LangSmith (external) | ❌ | ❌ |
| **Agent identity** | ✅ Agent Cards, A2A discovery | ❌ | ❌ | ❌ |
| **Runtime language** | Rust core + Python authoring | Python | Python | Python |
| **Best for** | Production multi-agent systems | Rapid prototyping | Conversational agents | Role-based crews |

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     Authoring Layer                       │
│              Python SDK  |  YAML  |  CLI                  │
├──────────────────────────────────────────────────────────┤
│                 Compilation / Validation                   │
│           Graph IR  |  Schema  |  Policy lint             │
├────────────────────────────┬─────────────────────────────┤
│      Rust Runtime Core     │      Protocol Layer          │
│  Scheduler  |  State SM    │  MCP Client  |  MCP Server   │
│  Event log  |  Snapshots   │  A2A Client  |  A2A Server   │
│  Workers    |  Timers      │                              │
├────────────────────────────┴─────────────────────────────┤
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
| 3 — Developer Delight | 🔄 In Progress | Eval harness, trace debugging, templates |
| 4 — Enterprise | 📋 Planned | Policy engine, tenant isolation, mTLS federation |
| 5 — Scale & Ecosystem | 📋 Planned | TypeScript SDK, hosted plane, agent marketplace |

---

## Documentation

Full documentation at **[jamjet.dev/docs](https://jamjet.dev/docs/quickstart)**

| | |
|--|--|
| [Quickstart](https://jamjet.dev/docs/quickstart) | Get running in 10 minutes |
| [Core Concepts](https://jamjet.dev/docs/concepts) | Agents, workflows, nodes, state, durability |
| [YAML Workflows](https://jamjet.dev/docs/yaml-workflows) | All node types, retry policies, conditions |
| [Python SDK](https://jamjet.dev/docs/python-sdk) | Full SDK reference |
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
├── runtime/
│   ├── core/           # Graph IR, node types, state machine
│   ├── scheduler/      # Durable task scheduler (Rust)
│   ├── workers/        # Node executors (model, tool, http, eval, …)
│   ├── api/            # REST API server
│   ├── agents/         # Agent Cards, registry, lifecycle
│   ├── protocols/
│   │   ├── mcp/        # MCP client + server
│   │   └── a2a/        # A2A client + server
│   └── telemetry/      # OTel instrumentation
├── sdk/
│   └── python/
│       └── jamjet/
│           ├── cli/    # jamjet CLI (Typer)
│           ├── eval/   # Eval dataset, runner, scorers
│           └── workflows/  # Python workflow builder
└── tests/              # Integration tests
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
  <sub>© 2026 JamJet — <a href="https://jamjet.dev">jamjet.dev</a></sub>
</div>
