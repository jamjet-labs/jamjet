<div align="center">

<h1>⚡ JamJet</h1>

**The open-source safety layer for AI agents.**

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

JamJet sits underneath your agent — LangChain, CrewAI, ADK, MCP servers, custom code — and enforces what prompts cannot:

- 🛡️ **Block unsafe tool calls** at runtime (database deletes, payments, file writes)
- ✋ **Pause for human approval** on risky actions, durably
- 💸 **Cap cost** per agent, per run, per project
- 📒 **Record an audit trail** that survives a regulator's review
- ⏪ **Replay or resume** crashed runs from the last checkpoint

**Keep your agent framework. Add JamJet when tool calls need control.**

```
            Your Agent / Framework
   (LangChain · CrewAI · ADK · custom · MCP client)
                     │
                     ▼
  ┌───────────────────────────────────────────────┐
  │            JamJet Safety Layer                │
  │   policy · approval · budget · audit · replay  │
  └───────────────────────────────────────────────┘
                     │
                     ▼
        Tools · MCP servers · APIs · DBs · Agents
```

> Prompts are not a security boundary. The runtime is.

→ Read **[When AI Deletes the Database](https://jamjet.dev/blog/when-ai-deletes-the-database/)** for why this is a runtime architecture problem, not a model problem.

![JamJet demo](./demo.gif)

## 60-second safety demo

```bash
pip install jamjet
```

Drop a policy beside your agent code. The runtime intercepts any matching tool call *before* it leaves the agent's process — `blocked_tools` are refused outright, `require_approval_for` pauses execution durably and waits for an out-of-band decision (crashes don't lose the approval; execution resumes when it arrives).

```yaml
# workflow.yaml
policy:
  blocked_tools:
    - "*delete*"
    - "payments.refund"
  require_approval_for:
    - "database.*"
    - "payment.transfer"
    - "user.suspend"
```

**Python, with the hosted control plane:**

```python
import jamjet
jamjet.cloud.configure(api_key="jj_...", project="my-agent")
jamjet.cloud.policy("block", "*delete*")
jamjet.cloud.policy("require_approval", "database.*")
# Every OpenAI / Anthropic call in this process is now policy-gated.
```

→ Runnable approval workflow in **[`examples/hitl-approval`](examples/hitl-approval)** · [Cloud Quickstart](https://docs.jamjet.dev/en/docs/cloud-quickstart)

## Use JamJet when your agent can…

- call MCP servers or arbitrary tools
- write to a database
- send emails or Slack messages
- trigger payments or external API calls
- access customer data or PII
- run for minutes/hours and needs to survive crashes
- spend real model budget at scale
- delegate to other agents

## What JamJet adds

| Without JamJet | With JamJet |
|---|---|
| Agent crashes lose progress | Resume from the last checkpoint |
| Tool calls rely on scattered app logic | Runtime policy blocks unsafe actions |
| Human approval is custom glue | Approval is a durable workflow step |
| Costs are discovered after the bill | Budgets enforced per agent / per run |
| Audit evidence is stitched from logs | Append-only event log, signed export |
| Memory is framework-specific | Engram works via MCP, REST, Python, Java |
| Frameworks stay siloed | MCP + A2A connect tools and agents |

## Works with your stack — not a replacement

JamJet does not replace LangChain, LangGraph, CrewAI, Google ADK, Spring AI, or your custom agent code. Use those to build agent behavior. Use JamJet to control what happens at runtime.

| You're using | Keep it for | JamJet adds |
|---|---|---|
| **LangChain · LangGraph · CrewAI · Google ADK · AutoGen** | Authoring agent behavior | Runtime safety: policy, audit, replay, approvals |
| **LangSmith · Arize · Weights & Biases** | Observability and evaluation | Active enforcement (block at runtime) + durable recovery |
| **Temporal · Orkes · DBOS** | General durable workflows | Agent-native primitives: policy on tool calls, MCP/A2A, memory |
| **Google · AWS · Azure agent platforms** | Cloud-native ecosystems | Open-source, cloud-neutral governance — works on-prem |

Community-built integrations for **LangChain, LlamaIndex, CrewAI, AutoGen, Pydantic-AI, DSPy, Spring AI, and LangChain4j** live in [`jamjet-labs/jamjet-examples/integrations`](https://github.com/jamjet-labs/jamjet-examples/tree/main/integrations). Want to build the official integration for *your* framework? **[Claim a slot](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration)** — first 10 merged contributors get JamJet swag.

## Examples

| Example | What it shows |
|---------|--------------|
| [`hitl-approval`](examples/hitl-approval) | Human approval as a first-class workflow primitive |
| [`coordinator-routing`](examples/coordinator-routing) | Dynamic agent routing with structured scoring |
| [`claims-processing`](examples/claims-processing) | Insurance pipeline — 4 specialist agents + HITL + audit |
| [`eval-harness`](examples/eval-harness) | Batch evaluation with LLM judge scoring |
| [`mcp-tool-consumer`](examples/mcp-tool-consumer) | Connect to external MCP tool servers |

→ [All 19 examples](examples/) · [Community integrations](https://github.com/jamjet-labs/jamjet-examples/tree/main/integrations) · [Build your own](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration)

## Sub-products

**[Engram](runtime/engram-server/README.md)** — JamJet's durable memory layer for agents (temporal knowledge graph, hybrid retrieval, conflict detection). Available as a [Rust crate](https://crates.io/crates/jamjet-engram), an [MCP server](https://registry.modelcontextprotocol.io/servers/io.github.jamjet-labs/engram-server) (Docker · GHCR), a [Python client](https://pypi.org/project/jamjet), and a [Spring AI `ChatMemoryRepository`](https://central.sonatype.com/artifact/dev.jamjet/engram-spring-boot-starter). Comparison with Mem0/Zep → [java-ai-memory.dev](https://java-ai-memory.dev).

**[JamJet Java Runtime](https://github.com/jamjet-labs/jamjet-runtime-java)** — embeds durable execution directly in your JVM, no Docker or sidecar, **8.9× faster** than calling out to one. Works with Spring AI, LangChain4j, and Google ADK. → [Launch post](https://jamjet.dev/blog/zero-sidecar-durable-agents-java/).

## Architecture

<details>
<summary><strong>Stack diagram</strong></summary>

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

</details>

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

*Hosted control plane available at [app.jamjet.dev](https://app.jamjet.dev) — traces, approval queue, audit retention, team projects. Optional. The runtime, both SDKs, and Engram are Apache-2.0 with no usage limits.*

### ⭐ Star JamJet if you believe agents need a runtime safety layer

<sub>Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet Labs · <a href="https://jamjet.dev">jamjet.dev</a> · Apache 2.0</sub>

</div>
