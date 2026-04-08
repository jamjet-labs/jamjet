<div align="center">

<!-- Lightning bolt logo — ⚡ fallback since GitHub strips inline SVG -->
<h1>⚡ JamJet</h1>

**The agent-native runtime — durable, composable, built for production.**

[![jamjet MCP server](https://glama.ai/mcp/servers/jamjet-labs/jamjet/badges/score.svg)](https://glama.ai/mcp/servers/jamjet-labs/jamjet)
[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange?style=flat-square)](https://rustup.rs)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Java](https://img.shields.io/badge/java-21%2B-red?style=flat-square)](https://openjdk.org)
[![Go](https://img.shields.io/badge/go-planned-lightgrey?style=flat-square)](https://go.dev)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev/quickstart)
[![Discord](https://img.shields.io/discord/1484398867611910305?style=flat-square&logo=discord&label=Discord&color=5865F2)](https://discord.gg/SAYnEj86fr)

[jamjet.dev](https://jamjet.dev) · [Quickstart](https://jamjet.dev/quickstart) · [Concepts](https://jamjet.dev/concepts) · [API Reference](https://jamjet.dev/api-reference) · [Examples](https://jamjet.dev/examples) · [Blog](https://jamjet.dev/blog) · [Discord](https://discord.gg/SAYnEj86fr)

[![Open in GitHub Codespaces](https://img.shields.io/badge/Open%20in-Codespaces-blue?style=flat-square&logo=github)](https://codespaces.new/jamjet-labs/jamjet?quickstart=1)
[![Open in Gitpod](https://img.shields.io/badge/Open%20in-Gitpod-orange?style=flat-square&logo=gitpod)](https://gitpod.io/#https://github.com/jamjet-labs/jamjet)

[![jamjet MCP server](https://glama.ai/mcp/servers/jamjet-labs/jamjet/badges/card.svg)](https://glama.ai/mcp/servers/jamjet-labs/jamjet)

</div>

<div align="center">

![JamJet demo](https://raw.githubusercontent.com/jamjet-labs/jamjet/main/demo.gif)

</div>

---

JamJet is a **performance-first, agent-native runtime** for AI agents. It is not another prompt wrapper or thin agent SDK — it is a **production-grade orchestration substrate** for agents that need to work, not just demo.

The runtime core is **Rust + Tokio** for scheduling, state, and concurrency. The authoring surface is **Python**, **Java**, **Go** (planned), or **YAML**. All compile to the same IR graph and run on the same engine.

## Why JamJet?

| Problem | JamJet's answer |
|---------|----------------|
| Agent runs lose state on crash | **Durable graph execution** — event-sourced, crash-safe resume |
| No way to pause for human approval | **Human-in-the-loop** as a first-class workflow primitive |
| Agents siloed in their own framework | **Native MCP + A2A** — interoperate with any agent, any framework |
| Slow Python orchestration at scale | **Rust core** — no GIL, real async parallelism |
| Weak observability, no replay | **Full event timeline**, OTel GenAI traces, replay from any checkpoint |
| No standard agent identity | **Agent Cards** — every agent is addressable and discoverable |
| Hard-coded agent routing | **Coordinator Node** — dynamic routing with structured scoring + LLM tiebreaker |
| Can't use agents as tools | **Agent-as-Tool** — wrap any agent as a callable tool (sync, streaming, conversational) |
| No governance or guardrails | **Policy engine** — tool blocking, approvals, autonomy enforcement, audit log |
| Agents with unchecked access | **OAuth delegation** — RFC 8693 token exchange, scope narrowing, per-step scoping |
| PII leaking into logs | **Data governance** — PII redaction (mask/hash/remove), retention policies, auto-purge |
| No tenant isolation | **Multi-tenant** — row-level partitioning, tenant-scoped state, isolated audit logs |
| Locked into one language | **Polyglot SDKs** — Python, Java (JDK 21), Go (planned), YAML — same IR, same runtime |
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

→ **[Full quickstart guide](https://jamjet.dev/quickstart)**

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

### Coordinator — dynamic agent routing

```python
from jamjet.coordinator import DefaultCoordinatorStrategy

strategy = DefaultCoordinatorStrategy(registry=my_registry)

# Discover agents by skill, score them, route to the best fit
candidates, _ = await strategy.discover(
    task="Analyze quarterly revenue data",
    required_skills=["data-analysis", "finance"],
    trust_domain="internal",
)
rankings, spread = await strategy.score(task, candidates, weights={})
decision = await strategy.decide(task, rankings, threshold=0.1)
# decision.selected_uri → "jamjet://org/finance-analyst"
```

### Agent-as-Tool — wrap agents as callable tools

```python
from jamjet.agent_tool import agent_tool

# Sync: quick, stateless
classifier = agent_tool(agent="jamjet://org/classifier", mode="sync",
                        description="Classifies documents by topic")

# Streaming: long-running with early termination on budget
researcher = agent_tool(agent="jamjet://org/researcher", mode="streaming",
                        description="Deep research with progress", budget={"max_cost_usd": 2.00})

# Conversational: multi-turn iterative refinement
reviewer = agent_tool(agent="jamjet://org/reviewer", mode="conversational",
                      description="Peer review with feedback", max_turns=5)
```

### Auto-routing — compiler inserts Coordinator automatically

```python
from jamjet.workflow.graph import WorkflowGraph

graph = WorkflowGraph("pipeline")
graph.add_agent_tool("process", agent="auto", mode="sync", output_key="result")
# ↑ "auto" expands at compile time into: Coordinator → AgentTool
ir = graph.compile()
# IR now has 2 nodes: _coordinator_process → process
```

---

## Agentic design patterns

JamJet supports the six major multi-agent orchestration patterns. Here's when to use each:

| Pattern | JamJet primitive | When to use | Example |
|---------|-----------------|-------------|---------|
| **Single Agent** | `Agent` with `@task` | Simple prototypes, single-purpose tasks | Chatbot, classifier |
| **Sequential Pipeline** | `WorkflowGraph` with edges | Ordered steps where each depends on the previous | ETL, document processing |
| **Parallel Fan-Out** | `ParallelNode` | Independent tasks that can run concurrently | Multi-source research, batch classification |
| **Loop & Critic** | `LoopNode` + `EvalNode` | Quality-critical tasks needing iterative refinement | Code review, content generation |
| **Coordinator (Dynamic Routing)** | `CoordinatorNode` | Route to the best agent at runtime based on capability, cost, latency | Support ticket routing, task delegation |
| **Agent-as-Tool** | `agent_tool()` wrapper | One agent needs to call another as a function | Orchestrator invoking specialists |

### Choosing the right pattern

```
Is it a single task?
  → Single Agent

Does order matter?
  → Sequential Pipeline

Can tasks run independently?
  → Parallel Fan-Out

Does output need quality checks?
  → Loop & Critic

Do you need to pick the best agent at runtime?
  → Coordinator

Does one agent need to invoke another?
  → Agent-as-Tool
```

### Coordinator vs static routing

| | Static (`ConditionalNode`) | Dynamic (`CoordinatorNode`) |
|---|---|---|
| **Candidates** | Declared in YAML | Discovered from registry at runtime |
| **Selection** | Expression-based rules | Structured scoring + optional LLM tiebreaker |
| **When agents change** | Redeploy workflow | Automatic — new agents discovered |
| **Observability** | Branch taken logged | Full scoring breakdown + reasoning in event log |
| **Best for** | Fixed, known routes | Dynamic environments, multi-tenant, research |

---

## How JamJet compares

> As of March 2026. All frameworks evolve — check their docs for the latest.

| Capability | JamJet | Google ADK | LangChain | AutoGen | CrewAI |
|------------|--------|------------|-----------|---------|--------|
| **Simple agent setup** | ✅ 3 lines (`@task`) | ✅ 5 lines | 6+ lines | 10+ lines | 8+ lines |
| **In-process execution** | ✅ `pip install` + run | ✅ native | ✅ native | ✅ native | ✅ native |
| **Durable execution** | ✅ event-sourced, crash-safe | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral | ❌ ephemeral |
| **Dynamic agent routing** | ✅ Coordinator with scoring + LLM tiebreaker | ✅ `transfer_to_agent()` | ❌ | ❌ | ❌ |
| **Agent-as-Tool** | ✅ sync, streaming, conversational | ✅ `AgentTool` (sync only) | ❌ | ❌ | ❌ |
| **Human-in-the-loop** | ✅ first-class primitive | 🟡 callbacks | 🟡 callbacks | 🟡 conversational | 🟡 manual |
| **MCP support** | ✅ client + server | ✅ client + server | 🟡 client only | 🟡 client only | 🟡 client only |
| **A2A protocol** | ✅ client + server | 🟡 client only | ❌ | ❌ | ❌ |
| **Built-in eval** | ✅ LLM judge, assertions, cost | ✅ 8 built-in criteria | ❌ | ❌ | ❌ |
| **Built-in observability** | ✅ OTel GenAI, event replay | ✅ Cloud Trace | 🟡 LangSmith (external) | ❌ | ❌ |
| **Agent identity** | ✅ Agent Cards, A2A discovery | ✅ Agent Cards | ❌ | ❌ | ❌ |
| **Policy & governance** | ✅ policy engine, audit log | 🟡 Model Armor plugin | ❌ | ❌ | ❌ |
| **Multi-tenant isolation** | ✅ row-level partitioning | ❌ | ❌ | ❌ | ❌ |
| **PII redaction** | ✅ mask/hash/remove, retention | 🟡 plugin | ❌ | ❌ | ❌ |
| **Model independence** | ✅ any model provider | 🟡 Gemini-first (LiteLLM escape) | ✅ any | ✅ any | ✅ any |
| **Progressive complexity** | ✅ `@task` → `Agent` → `Workflow` | 🟡 code or YAML | ❌ single API | ❌ | ❌ |
| **Managed deployment** | 📋 Planned | ✅ Vertex AI Agent Engine | ❌ | ❌ | ❌ |
| **Runtime language** | Rust core + Python/Java/Go | Python/TS/Go/Java | Python | Python | Python |
| **Best for** | Production multi-agent systems | Google Cloud AI agents | Rapid prototyping | Conversational agents | Role-based crews |

---

## Memory — Engram

JamJet ships with **Engram**, a durable memory layer for agents — temporal knowledge graph, hybrid retrieval, consolidation engine, all backed by a single SQLite file. Engram runs as an embedded Rust library or as a standalone MCP/REST server, and is consumable from Python, Java, and Spring Boot.

**Provider-agnostic.** The same Engram binary speaks to Ollama (local, free), any OpenAI-compatible endpoint (OpenAI, Azure, Groq, Together, Mistral, DeepSeek, Perplexity, OpenRouter, vLLM, LM Studio, …), Anthropic Claude, Google Gemini, or a `command` shell-out for anything else — pick one with `ENGRAM_LLM_PROVIDER=…`, no recompile.

| Shape | Package | When to use |
|---|---|---|
| Rust library | `jamjet-engram` (crates.io) | Embedding memory directly in a Rust application |
| Standalone binary | `jamjet-engram-server` (crates.io), `ghcr.io/jamjet-labs/engram-server` (Docker), [Official MCP Registry](https://registry.modelcontextprotocol.io/servers/io.github.jamjet-labs/engram-server) | MCP clients (Claude Desktop, Cursor), language-agnostic REST clients, zero-code setups |
| Python client | `jamjet` (PyPI) | Python agents talking to `engram-server` over REST |
| Java client | `dev.jamjet:jamjet-sdk` (Maven Central) | JVM agents talking to `engram-server` over REST |
| Spring Boot starter | `dev.jamjet:engram-spring-boot-starter` (Maven Central) | Drop-in `@Bean EngramMemory` for Spring AI applications |

```bash
# Try it with Claude Desktop in 30 seconds (uses local Ollama by default)
docker run --rm -i \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2

# Or point at Groq instead — same binary, no rebuild
docker run --rm -i \
  -e ENGRAM_LLM_PROVIDER=openai-compatible \
  -e ENGRAM_OPENAI_BASE_URL=https://api.groq.com/openai/v1 \
  -e OPENAI_API_KEY=gsk_... \
  -v engram-data:/data \
  ghcr.io/jamjet-labs/engram-server:0.3.2
```

Seven MCP tools exposed by the server: `memory_add`, `memory_recall`, `memory_context`, `memory_search`, `memory_forget`, `memory_stats`, `memory_consolidate`. Full docs at [**runtime/engram-server/README.md**](runtime/engram-server/README.md). For how Engram compares to Mem0, Zep, Spring AI ChatMemory, LangChain4j, Koog, Google ADK Memory Bank, and Embabel, see [java-ai-memory.dev](https://java-ai-memory.dev).

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     Authoring Layer                       │
│     Python SDK  |  Java SDK  |  Go SDK (planned)  |  YAML  │
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
│  Policy  |  Audit  |  PII Redaction  |  OAuth  |  mTLS     │
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
| 4 — Enterprise | 🔄 In Progress | Policy engine, tenant isolation, PII redaction, OAuth delegation, A2A federation auth, mTLS |
| 5 — Scale & Ecosystem | 📋 Planned | Go SDK, TypeScript SDK, hosted plane, agent marketplace |

---

## Documentation

Full documentation at **[jamjet.dev](https://jamjet.dev/quickstart)**

| | |
|--|--|
| [Quickstart](https://jamjet.dev/quickstart) | Get running in 10 minutes |
| [Core Concepts](https://jamjet.dev/concepts) | Agents, workflows, nodes, state, durability |
| [YAML Workflows](https://jamjet.dev/yaml-workflows) | All node types, retry policies, conditions |
| [Python SDK](https://jamjet.dev/python-sdk) | Full Python SDK reference |
| [Java SDK](https://jamjet.dev/java-sdk) | Builders, records, @Tool annotation, agents |
| [REST API](https://jamjet.dev/api-reference) | All endpoints, auth, request/response schemas |
| [Enterprise Security](https://jamjet.dev/enterprise) | Tenants, PII redaction, OAuth, mTLS federation |
| [MCP Integration](https://jamjet.dev/mcp) | Connect to MCP servers, expose tools |
| [A2A Integration](https://jamjet.dev/a2a) | Delegate to and serve external agents |
| [Eval Harness](https://jamjet.dev/eval) | Score quality, run regression suites, gate CI |
| [Observability](https://jamjet.dev/observability) | OTel traces, metrics, Prometheus |
| [Deployment](https://jamjet.dev/deployment) | Docker, Kubernetes, PostgreSQL |
| [CLI Reference](https://jamjet.dev/cli) | Full CLI reference |

---

## Repository structure

```
jamjet/
├── runtime/                # Rust workspace
│   ├── core/               # Graph IR, node types, state machine
│   ├── ir/                 # Canonical Intermediate Representation
│   ├── scheduler/          # Durable task scheduler
│   ├── state/              # Event-sourced state, snapshots
│   ├── workers/            # Node executors (model, tool, http, eval, …)
│   ├── api/                # REST API, OAuth delegation, secrets backends
│   ├── agents/             # Agent Cards, registry, lifecycle
│   ├── models/             # LLM provider adapter layer
│   ├── timers/             # Durable timers, Postgres-backed cron
│   ├── policy/             # Policy engine, PII redaction
│   ├── audit/              # Immutable audit log
│   ├── engram/             # Durable memory library (jamjet-engram crate)
│   ├── engram-server/      # MCP + REST server binary (jamjet-engram-server)
│   ├── protocols/
│   │   ├── mcp/            # MCP client + server
│   │   └── a2a/            # A2A client + server + federation auth + mTLS
│   └── telemetry/          # OTel instrumentation
├── sdk/
│   ├── python/             # Python SDK + CLI
│   │   └── jamjet/
│   │       ├── cli/        # jamjet CLI (Typer)
│   │       ├── eval/       # Eval dataset, runner, scorers
│   │       ├── agents/     # Agent definitions + strategies
│   │       ├── templates/  # Project scaffolding templates
│   │       └── workflow/   # Python workflow builder
│   ├── java/               # Java SDK (JDK 21, virtual threads, records)
│   │   ├── jamjet-sdk/     # Core SDK module
│   │   └── jamjet-cli/     # CLI module
│   └── go/                 # Go SDK (planned — Phase 5)
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
