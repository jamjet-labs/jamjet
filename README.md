<div align="center">

<h1>⚡ JamJet</h1>

**The safety layer behind your AI agents.**

[![jamjet MCP server](https://glama.ai/mcp/servers/jamjet-labs/jamjet/badges/score.svg)](https://glama.ai/mcp/servers/jamjet-labs/jamjet)
[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![crates.io](https://img.shields.io/crates/v/jamjet-engram?style=flat-square&color=f5c518)](https://crates.io/crates/jamjet-engram)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](LICENSE)
[![GitHub stars](https://img.shields.io/github/stars/jamjet-labs/jamjet?style=flat-square&color=f5c518)](https://github.com/jamjet-labs/jamjet/stargazers)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square)](https://rustup.rs)
[![Python](https://img.shields.io/badge/python-3.11%2B-blue?style=flat-square)](https://python.org)
[![Java](https://img.shields.io/badge/java-21%2B-red?style=flat-square)](https://openjdk.org)
[![Docs](https://img.shields.io/badge/docs-jamjet.dev-f5c518?style=flat-square)](https://jamjet.dev/quickstart)
[![Discord](https://img.shields.io/discord/1484398867611910305?style=flat-square&logo=discord&label=Discord&color=5865F2)](https://discord.gg/SAYnEj86fr)

[jamjet.dev](https://jamjet.dev) · [Quickstart](https://jamjet.dev/quickstart) · [Docs](https://jamjet.dev/concepts) · [Examples](https://jamjet.dev/examples) · [Blog](https://jamjet.dev/blog) · [Discord](https://discord.gg/SAYnEj86fr)

[![Open in GitHub Codespaces](https://img.shields.io/badge/Open%20in-Codespaces-blue?style=flat-square&logo=github)](https://codespaces.new/jamjet-labs/jamjet?quickstart=1)
[![Open in Gitpod](https://img.shields.io/badge/Open%20in-Gitpod-orange?style=flat-square&logo=gitpod)](https://gitpod.io/#https://github.com/jamjet-labs/jamjet)

</div>

---

AI agents fail in predictable ways: they lose state on crash, call tools they shouldn't, skip human approval, run past budget, forget context, or leave no useful audit trail.

JamJet is an open-source runtime that handles those failure modes directly: policy checks, audit trails, human approval, crash recovery, cost limits, durable memory, and MCP/A2A interoperability.

**Keep your framework. Add JamJet when an agent needs to be replayable, auditable, and controlled.**

> *89% of enterprise agents never reach production. JamJet exists to fix that.* — for developers shipping agents that need to survive crashes, unsafe tool calls, runaway costs, missed approvals, and audit gaps.

![JamJet demo](./demo.gif)

## What JamJet adds

| Without JamJet | With JamJet |
|---|---|
| Agent crashes lose progress | Resume from the last checkpoint |
| Tool calls rely on scattered app logic | Runtime policies block unsafe actions |
| Human approval is custom glue code | Approval is a durable workflow step |
| Costs are discovered after the bill | Budgets enforced per agent / per run |
| Audit evidence is stitched from logs | Every run has an execution trail |
| Memory is framework-specific | Engram works via MCP, REST, Python, Java |
| Agent frameworks stay isolated | MCP + A2A connect tools and agents |

## Quickstart

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
```

No server. No config. No YAML. Just `pip install` and run. → **[Full quickstart](https://jamjet.dev/quickstart)**

## Add the safety layer

Add JamJet Cloud's two-line config and the same code gains policy enforcement, cost budgets, human approval, and an audit trail:

```python
import jamjet.cloud as jamjet
from openai import OpenAI

jamjet.configure(api_key="jj_...", project="my-agent")
jamjet.policy("require_approval", "payments.*")   # risky tools wait for human OK
jamjet.budget(max_cost_usd=5.00)                  # cap spend per run

# Every OpenAI / Anthropic call is now traced, policy-checked, and counted toward the budget.
OpenAI().chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "..."}],
)
```

Every run shows up in `app.jamjet.dev/dashboard/traces` within ~5 seconds — model, tokens, cost, duration, every tool call. → [**Cloud Quickstart**](https://docs.jamjet.dev/en/docs/cloud-quickstart)

## Why JamJet?

| Problem | JamJet's answer |
|---------|----------------|
| Unsafe tool calls slip through | **Policy engine** — block, allow, or require approval per tool pattern |
| No audit trail of agent decisions | **Append-only event log** + audit export |
| Agent crashes lose progress | **Event-sourced execution** with checkpoint resume |
| Human approval is custom glue | **First-class pause/resume** approval step |
| Costs run away on long agents | **Per-run / per-agent budgets** with enforcement |
| Memory is framework-specific | **Engram** memory via MCP, REST, Python, Java |
| Agents stuck in one framework | **MCP + A2A** client + server interoperability |
| Python orchestration bottlenecks | **Rust + Tokio** runtime; Python and Java SDKs |
| Need a hosted control plane | **JamJet Cloud** — drop-in two lines for traces, policy queue, audit retention. See [Cloud Quickstart](https://docs.jamjet.dev/en/docs/cloud-quickstart). |

## Progressive Complexity

Three levels of abstraction — all compile to the same IR and run on the same engine.

**`@task` — one function, zero boilerplate**

```python
@task(model="claude-haiku-4-5-20251001", tools=[web_search])
async def research(question: str) -> str:
    """You are a research assistant."""
```

**`Agent` — explicit configuration**

```python
agent = Agent("researcher", model="claude-haiku-4-5-20251001",
              tools=[web_search], instructions="Search first, then summarize.")
result = await agent.run("What is JamJet?")
```

**`Workflow` — full graph control**

```python
workflow = Workflow("research")

@workflow.step
async def search(state: State) -> State:
    result = await web_search(query=state.query)
    return state.model_copy(update={"answer": result})
```

**YAML — declarative workflows**

```yaml
nodes:
  think:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: "Answer clearly: {{ state.query }}"
    output_key: answer
    next: end
```

## Key Capabilities

**Coordinator — dynamic agent routing.** Discover agents by skill, score them, route to the best fit at runtime. Structured scoring with optional LLM tiebreaker, full scoring breakdown in event logs. [Example →](examples/coordinator-routing)

**Agent-as-Tool.** Wrap any agent as a callable tool — sync (quick, stateless), streaming (long-running with budget limits), or conversational (multi-turn with turn limits). [Example →](examples/agent-as-tool)

**MCP — client + server.** Connect to external MCP tool servers in workflows, or expose JamJet tools as an MCP server for Claude Desktop, Cursor, and other clients. [Example →](examples/mcp-tool-consumer)

**A2A protocol — client + server.** Delegate tasks to external agents or serve tasks from other frameworks via Agent-to-Agent protocol. [Example →](examples/a2a-delegation)

**Eval harness.** Built-in LLM judge, assertions, cost scoring. Self-improvement loop with `on_fail: retry_with_feedback`. [Example →](examples/eval-harness)

**Human-in-the-loop.** First-class approval primitive — pause execution, collect human input, resume. [Example →](examples/hitl-approval)

## Memory — Engram

**Engram** is JamJet's durable memory layer — temporal knowledge graph, hybrid retrieval, fact extraction, conflict detection, and consolidation. Backed by **SQLite** (zero-infra) or **PostgreSQL** (production). Ships with a built-in **message store** for conversation history.

**Provider-agnostic.** One binary speaks to Ollama (local, free), any OpenAI-compatible endpoint (OpenAI, Azure, Groq, Together, DeepSeek, …), Anthropic Claude, Google Gemini, or a shell-out command — set `ENGRAM_LLM_PROVIDER=…` and go.

| Package | Install from | Use case |
|---|---|---|
| `jamjet-engram` | [crates.io](https://crates.io/crates/jamjet-engram) | Embed in Rust apps |
| `jamjet-engram-server` | [crates.io](https://crates.io/crates/jamjet-engram-server) · [Docker](https://ghcr.io/jamjet-labs/engram-server) · [MCP Registry](https://registry.modelcontextprotocol.io/servers/io.github.jamjet-labs/engram-server) | MCP + REST server |
| `jamjet` | [PyPI](https://pypi.org/project/jamjet) | Python client |
| `dev.jamjet:jamjet-sdk` | [Maven Central](https://central.sonatype.com/artifact/dev.jamjet/jamjet-sdk) | Java client |
| `dev.jamjet:engram-spring-boot-starter` | [Maven Central](https://central.sonatype.com/artifact/dev.jamjet/engram-spring-boot-starter) | Spring AI `ChatMemoryRepository` |

```bash
# Try with Claude Desktop — uses local Ollama by default
docker run --rm -i -v engram-data:/data ghcr.io/jamjet-labs/engram-server:0.5.0
```

11 MCP tools: `memory_add`, `memory_recall`, `memory_context`, `memory_search`, `memory_forget`, `memory_stats`, `memory_consolidate`, `messages_save`, `messages_get`, `messages_list`, `messages_delete`.

Full docs → [runtime/engram-server/README.md](runtime/engram-server/README.md) · Comparison with Mem0, Zep, and others → [java-ai-memory.dev](https://java-ai-memory.dev)

## Java Runtime — durable agents native to your JVM

**Spring AI? LangChain4j? Plain Java?** The [JamJet Java Runtime](https://github.com/jamjet-labs/jamjet-runtime-java) embeds durable execution directly in your JVM. No Docker, no sidecar, no REST overhead — and **8.9× faster** than calling out to one.

```java
@DurableAgent
@Service
public class MyAgent {
    @Checkpoint("search")
    public String search(String topic) {
        return chatClient.prompt("Research: " + topic).call().content();
    }
}
// Kill the process. Restart. It resumes from the last checkpoint.
```

```xml
<dependency>
    <groupId>dev.jamjet</groupId>
    <artifactId>jamjet-runtime-spring-boot-starter</artifactId>
    <version>0.1.1</version>
</dependency>
```

Virtual threads, MCP native, plugin hot-reload. Works with Spring AI, LangChain4j, Google ADK. [Read the launch post](https://jamjet.dev/blog/zero-sidecar-durable-agents-java/).

## JamJet Cloud — shared control plane for teams

**Free OSS forever.** This runtime, Engram local, both SDKs, and the Java runtime are Apache-2.0 — no usage limits, no telemetry.

JamJet Cloud is the optional hosted layer for teams that need shared visibility:

- **Traces** — every run with model, tokens, cost, duration, every tool call
- **Policy violations + approval queue** — human OKs in one place
- **Budgets + cost attribution** — per agent, per project, per environment
- **Hosted Engram memory** — multi-tenant, no infrastructure
- **Audit retention + export** — evidence-grade logs for compliance
- **Team projects + environments** — dev / staging / prod isolation

Two lines to wire it in:

```python
import jamjet.cloud as jamjet
jamjet.configure(api_key="jj_...", project="my-agent")
```

Multi-agent network graph + Java cloud SDK ship Q3 2026. → [**Cloud Quickstart**](https://docs.jamjet.dev/en/docs/cloud-quickstart) · [**Sign up**](https://app.jamjet.dev)

## How JamJet fits with your existing stack

The strategy is simple: keep your framework, add JamJet for runtime safety.

| Tool category | Use it for | JamJet adds |
|---|---|---|
| **LangChain · LangGraph · CrewAI · Google ADK** | Authoring agent behavior | Runtime safety: policy, audit, replay, approvals |
| **LangSmith · Arize · Weights & Biases** | Observability and evaluation | Active enforcement (block at runtime) + durable recovery |
| **Temporal · Orkes · DBOS** | General durable workflows | Agent-native primitives: policy on tool calls, MCP/A2A, memory |
| **Google · AWS · Azure agent platforms** | Cloud-native agent ecosystems | Open-source, cloud-neutral governance — works on-prem |

JamJet is the runtime safety layer underneath whatever framework you already use.

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
│  Model Adapters  |  Tool Execution  |  Engram Memory      │
├──────────────────────────────────────────────────────────┤
│                         Storage                           │
│           Postgres (production)  |  SQLite (local)        │
└──────────────────────────────────────────────────────────┘
```

## Community Integrations

JamJet works with your existing AI framework. Browse community-built
integrations for LangChain, LlamaIndex, CrewAI, AutoGen, Pydantic-AI, DSPy,
Spring AI, and LangChain4j → [`jamjet-labs/jamjet-examples/integrations`](https://github.com/jamjet-labs/jamjet-examples/tree/main/integrations).

**Want to build the official integration for *your* framework?**
[Claim a slot](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration)
— first 10 merged contributors get JamJet swag.

## Examples

**Recommended starting points:**

| Example | What it shows |
|---------|--------------|
| [hitl-approval](examples/hitl-approval) | Human approval as a first-class workflow primitive |
| [coordinator-routing](examples/coordinator-routing) | Dynamic agent routing with structured scoring |
| [claims-processing](examples/claims-processing) | Insurance pipeline — 4 specialist agents + HITL + audit |
| [eval-harness](examples/eval-harness) | Batch evaluation with LLM judge scoring |
| [mcp-tool-consumer](examples/mcp-tool-consumer) | Connect to external MCP tool servers |

→ [All 19 examples](examples/) · [Community integrations](https://github.com/jamjet-labs/jamjet-examples/tree/main/integrations) · [Build your own](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration)

## Roadmap

| Phase | Status | Goal |
|-------|--------|------|
| 0 — Architecture & RFCs | ✅ Complete | Design docs, RFCs, repo scaffolding |
| 1 — Minimal Viable Runtime | ✅ Complete | Local durable execution, MCP client, agent cards |
| 2 — Production Core | ✅ Complete | Distributed workers, MCP server, A2A client + server |
| 3 — Developer Delight | 🔄 In Progress | Eval harness, ~~Java SDK~~ ✅, **[Java Runtime](https://github.com/jamjet-labs/jamjet-runtime-java)** ✅, trace debugging, templates |
| 4 — Enterprise | 🔄 In Progress | Policy engine, tenant isolation, PII redaction, OAuth, mTLS |
| 5 — Scale & Ecosystem | 📋 Planned | Go SDK, TypeScript SDK, hosted plane, marketplace |

## Documentation

Full docs at **[jamjet.dev](https://jamjet.dev/quickstart)**

[Quickstart](https://jamjet.dev/quickstart) · [Concepts](https://jamjet.dev/concepts) · [Python SDK](https://jamjet.dev/python-sdk) · [Java SDK](https://jamjet.dev/java-sdk) · [YAML Workflows](https://jamjet.dev/yaml-workflows) · [REST API](https://jamjet.dev/api-reference) · [MCP](https://jamjet.dev/mcp) · [A2A](https://jamjet.dev/a2a) · [Eval](https://jamjet.dev/eval) · [Enterprise](https://jamjet.dev/enterprise) · [Observability](https://jamjet.dev/observability) · [CLI](https://jamjet.dev/cli) · [Deployment](https://jamjet.dev/deployment)

## Repository Structure

```
jamjet/
├── runtime/                # Rust workspace
│   ├── core/               # Graph IR, node types, state machine
│   ├── scheduler/          # Durable task scheduler
│   ├── state/              # Event-sourced state, snapshots
│   ├── workers/            # Node executors (model, tool, eval, …)
│   ├── api/                # REST API, OAuth delegation
│   ├── engram/             # Durable memory library (crates.io)
│   ├── engram-server/      # MCP + REST memory server
│   ├── protocols/          # MCP + A2A client/server
│   └── ...                 # agents, models, timers, policy, audit, telemetry
├── sdk/
│   ├── python/             # Python SDK + CLI (PyPI)
│   ├── java/               # Java SDK (Maven Central)
│   └── go/                 # Go SDK (planned)
└── examples/               # 19 runnable examples
```

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

**Looking for a starter task?**
- Build a [framework integration](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration) — 8 slots open, first 10 contributors get JamJet swag
- Browse [good first issues](https://github.com/jamjet-labs/jamjet/labels/good%20first%20issue)
- Join the conversation in [Discord](https://discord.gg/SAYnEj86fr)

## Community

[GitHub Discussions](https://github.com/jamjet-labs/jamjet/discussions) · [Issues](https://github.com/jamjet-labs/jamjet/issues) · [Discord](https://discord.gg/SAYnEj86fr)

## License

Apache 2.0 — see [LICENSE](LICENSE).

---

<div align="center">

### ⭐ Star JamJet if you believe agents need a safety layer

<sub>Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet Labs · <a href="https://jamjet.dev">jamjet.dev</a> · Apache 2.0</sub>

</div>
