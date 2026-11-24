<div align="center">

<h1>⚡ JamJet</h1>

**The action-control plane for AI agents.** One policy file. One audit trail. Across hooks, guardrails, MCP gateways, SDKs, and custom runtimes.

[![jamjet MCP server](https://glama.ai/mcp/servers/jamjet-labs/jamjet/badges/score.svg)](https://glama.ai/mcp/servers/jamjet-labs/jamjet)
[![CI](https://img.shields.io/github/actions/workflow/status/jamjet-labs/jamjet/ci.yml?label=CI&style=flat-square)](https://github.com/jamjet-labs/jamjet/actions)
[![PyPI](https://img.shields.io/pypi/v/jamjet?style=flat-square&color=f5c518)](https://pypi.org/project/jamjet)
[![Maven Central](https://img.shields.io/maven-central/v/dev.jamjet/jamjet-runtime-core?style=flat-square&color=f5c518&label=maven)](https://central.sonatype.com/artifact/dev.jamjet/jamjet-runtime-core)
[![crates.io](https://img.shields.io/crates/v/jamjet-policy?style=flat-square&color=f5c518&label=crates.io)](https://crates.io/crates/jamjet-policy)
[![License](https://img.shields.io/badge/license-Apache%202.0-f5c518?style=flat-square)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-join-5865F2?style=flat-square&logo=discord&logoColor=white)](https://discord.gg/SAYnEj86fr)

[jamjet.dev](https://jamjet.dev) · [Quickstart](https://docs.jamjet.dev/en/docs/open-source/quickstart) · [Docs](https://docs.jamjet.dev) · [Examples](examples/) · [Videos](https://www.youtube.com/playlist?list=PLD5Bg9vUNgik) · [Blog](https://jamjet.dev/blog) · [Discord](https://discord.gg/SAYnEj86fr)

[![Open in GitHub Codespaces](https://img.shields.io/badge/Open%20in-Codespaces-blue?style=flat-square&logo=github)](https://codespaces.new/jamjet-labs/jamjet?quickstart=1)
[![Open in Gitpod](https://img.shields.io/badge/Open%20in-Gitpod-orange?style=flat-square&logo=gitpod)](https://gitpod.io/#https://github.com/jamjet-labs/jamjet)

</div>

---

> *Write the safety policy once. Run it everywhere your agents can act.*

JamJet sits underneath your agent (Claude Code, OpenAI Agents SDK, MCP clients, LangChain, CrewAI, ADK, Spring AI, custom code) and enforces what prompts cannot:

- 🛡️ **Block unsafe tool calls** at runtime: database deletes, payments, file writes
- ✋ **Pause for human approval** on risky actions, durably
- 💸 **Cap tokens and dollars** per run, counted from the run's own event log
- 📒 **Record an audit trail** of every decision, with approvals signed and hash-chained
- ⏪ **Replay or resume** crashed runs from the last checkpoint

**Keep your agent framework. Add JamJet where tool calls need control.**

## Watch it built

The code-along builds one venue-booking agent, a step at a time. Each part is six to seven minutes, and every step has its own folder in [`examples/venue-booking-agent`](examples/venue-booking-agent).

<table>
<tr>
<td width="50%"><a href="https://www.youtube.com/watch?v=VVLrZosEwhE"><img src="https://img.youtube.com/vi/VVLrZosEwhE/maxresdefault.jpg" alt="Video: Build your first AI agent, JamJet Python code-along part 1"></a></td>
<td width="50%"><a href="https://www.youtube.com/watch?v=tnqfI4vZt-g"><img src="https://img.youtube.com/vi/tnqfI4vZt-g/maxresdefault.jpg" alt="Video: Make your AI agent ask before it pays, JamJet Python code-along part 2"></a></td>
</tr>
<tr>
<td valign="top"><b><a href="https://www.youtube.com/watch?v=VVLrZosEwhE">Part 1: Build your first AI agent</a></b><br>An agent with instructions and two tools, then a <code>pay_deposit</code> tool that pays because the prompt said so.</td>
<td valign="top"><b><a href="https://www.youtube.com/watch?v=tnqfI4vZt-g">Part 2: Make it ask before it pays</a></b><br>A budget cap, an approval before <code>pay_deposit</code> runs, and a tool the agent is never allowed to call.</td>
</tr>
</table>

All parts are in the [code-along playlist](https://www.youtube.com/playlist?list=PLD5Bg9vUNgik).

## See it in 60 seconds

```bash
pip install jamjet
jamjet demo unsafe-tool-call
```

No API key. No Docker. No cloud account. The model is mocked; the policy check is the SDK's own evaluator. The demos run in process and do not start the runtime. Three more demos run the same way:

```bash
jamjet demo approval        # pause-for-approval flow
jamjet demo budget-cap      # $0.05 cost cap
jamjet demo mcp-tool-policy # policy on an MCP-shaped request (no MCP server involved)
```

## Run an agent with no API key

Set `model="scripted/<path to a YAML file>"` and each model call is answered from
the file. Only the model is scripted. Policy, approval, budget and durability are
the real ones.

```yaml
# scripts/book.yaml: turn N answers the agent's Nth model call
turns:
  - tool_calls:
      - name: search_venues
        arguments: {city: Lisbon, date: "2026-12-11"}
  - tool_calls:
      - name: check_dates
        arguments: {venue: Casa do Rio}
  - tool_calls:
      - name: pay_deposit
        arguments: {venue: Casa do Rio, amount: 500}
  - content: "Booked Casa do Rio in Lisbon for 11 December. The 500 deposit is paid."
```

```python
# book.py
import asyncio
from jamjet import Agent
from venue_tools import search_venues, check_dates, pay_deposit  # plain `def` functions under @tool

agent = Agent(
    "venue-booker",
    model="scripted/scripts/book.yaml",
    tools=[search_venues, check_dates, pay_deposit],
    instructions="You book event venues. Use the tools.",
    strategy="react",
    policy={"require_approval_for": ["pay_deposit"], "blocked_tools": ["cancel_*"]},
)

if __name__ == "__main__":
    result = asyncio.run(agent.run_durable("Book a venue in Lisbon for 2026-12-11."))
    print(result.output, result.execution_id)
```

A relative script path is resolved from the working directory of the process that
reads it. Run `jamjet dev` from your project folder, or give an absolute path.

Run `jamjet dev --modules venue_tools` in one terminal and `python book.py` in
another. The run waits at `pay_deposit`. Then:

```bash
jamjet executions                    # every run
jamjet approvals                     # every run waiting for approval
jamjet approve <id> -d approved --as <name>
jamjet audit <id>
```

On a real run, `jamjet audit` ended with:

```
pay_deposit ran 1 time, approved by tester at 03:09:24
```

The full walkthrough, with the output of each command, is in the
[Python guide](sdk/python/README.md). Python 3.11 or newer.
`pip install jamjet` is enough for in-process runs on the scripted model.
`jamjet dev` and durable runs need `pip install 'jamjet[sidecar]'`.
Real providers need `jamjet[model]` as well.

## Run a crew of agents as jobs

Engine 0.6 and Python SDK 0.14 let a crew of agents run as jobs on a serverless container platform such as Cloud Run, with the engine beside them on Postgres. We built these pieces for an agent crew moving to Cloud Run and Cloud SQL.

Each stage of the crew is one agent. A job starts its run and works the run's steps itself. When a person has to decide, the job exits. Started again with the same key, it picks up the same run.

```python
# job.py: one stage of the crew. Run it as a job, and again after a decision.
import asyncio, sys
from jamjet.drive import RunWaiting

from book import agent  # the venue agent above

async def main() -> int:
    try:
        result = await agent.run_durable(
            "Book a venue in Lisbon for 2026-12-11.",
            run_key="booking:lisbon:2026-12-11",  # a retried job finds this run
            drive=True,                           # this process works the run's steps
        )
    except RunWaiting as w:
        print(f"waiting for {w.waiting_for} on {w.execution_id}")
        return 0  # not a failure: start the job again after the decision
    print(result.output)
    return 0

sys.exit(asyncio.run(main()))
```

Against `jamjet dev --modules venue_tools`, the first start runs the model and tool steps in the job and stops at the approval:

```
waiting for approval on exec_131df6a6b82147f7866d54eea084f6c4
```

After `jamjet approve`, the same job with the same key finishes that run. `jamjet executions` lists one run, and `jamjet audit` ends with `pay_deposit ran 1 time, approved by reviewer`.

| A crew needs | What JamJet does |
|---|---|
| A retried job must not start a second run | A start with the same `run_key` returns the run the first start made |
| No worker fleet to keep alive | With `drive=True`, the process that starts the run claims its tool and model steps |
| Model calls through your own router | `model=Model(backend=...)` sends each call to your router. The engine still holds the answer to the run's model allow list and caps |
| Spend that cannot run away | `token_budget` and `cost_budget_usd` per run. The next step that would spend past a cap is refused and the run fails |
| A reviewer who picks, not only approves | `options` on a `human_approval` node. The choice and the note are sealed into the audit log and written to `state.__approvals`, so a condition can route on them |
| An app that shows what is waiting | `GET /approvals`, or `client.list_pending_approvals()`, lists pending approvals across runs |
| Nothing running between requests | `JAMJET_BACKGROUND=off` turns the engine's loops off, and a scheduler calls `POST /ops/tick` |
| State that outlives an instance | The Postgres store (preview), with `jamjet-server migrate` before each deploy |

A stage where the reviewer picks between options, with caps on the run:

```yaml
cost_budget_usd: 2.50
token_budget: { total_tokens: 50000 }

nodes:
  pick_topic:
    type: human_approval
    description: "Which topic goes out this week?"
    options:
      - {id: topic_1, label: "Error handling in Rust"}
      - {id: topic_2, label: "Durable timers"}
      - {id: revise, label: "None of these, revise the list"}
    next: route   # a condition node that reads state.__approvals.pick_topic.choice
```

```bash
jamjet approve <id> -d approved --node-id pick_topic --choice revise --note "Too close to last week" --as <name>
```

The [deployment guide](docs/guides/deployment.md#on-a-serverless-container-platform) has the platform settings, [request-driven mode](docs/guides/deployment.md#request-driven-mode) and [Postgres](docs/guides/deployment.md#postgres-preview). The [Python guide](docs/guides/python-sdk.md#driving-a-run-from-your-own-process) covers drive mode, run keys, your own model router, and what a job should do with each way a run can end.

## On the JVM

**[JamJet Java Runtime](https://github.com/jamjet-labs/jamjet-runtime-java)** embeds durable execution directly in your JVM process: Java 21, no Docker, **8.9× faster** than calling out to a REST sidecar ([benchmark and launch post](https://jamjet.dev/blog/zero-sidecar-durable-agents-java/)). Works with Spring AI, LangChain4j, and Google ADK.

→ See a Spring Boot agent survive `kill -9` mid-run in **[`examples/loan-underwriter-agent`](examples/loan-underwriter-agent)**: resumes from disk checkpoints, gates disbursement on human approval, emits a signed receipt bundle.

To record Spring AI calls on this runtime over HTTP instead, see [jamjet-spring](https://github.com/jamjet-labs/jamjet-spring). Its 0.1.0 release on Maven Central calls routes this runtime does not serve and records nothing.

## The same policy, everywhere

Every agent toolchain is inventing its own safety layer. JamJet gives you one policy file and one audit trail across all of them.

| Adapter | Install | Host |
|---|---|---|
| [`@jamjet/claude-code-hook`](https://npmjs.com/package/@jamjet/claude-code-hook) | `npm i -g @jamjet/claude-code-hook` | [Claude Code](https://docs.claude.com/en/docs/claude-code/hooks) PreToolUse hook |
| [`@jamjet/mcp-shim`](https://npmjs.com/package/@jamjet/mcp-shim) | `npx -y @jamjet/mcp-shim ...` | Any [MCP](https://modelcontextprotocol.io) client (Claude Desktop, Cursor, …) |
| [`@jamjet/openai-guardrail`](https://npmjs.com/package/@jamjet/openai-guardrail) | `npm i @jamjet/openai-guardrail` | [OpenAI Agents SDK](https://github.com/openai/openai-agents-js) tool guardrail (TS) |
| [`jamjet.integrations.openai_guardrail`](https://pypi.org/project/jamjet/) | `pip install jamjet` | [OpenAI Agents SDK](https://github.com/openai/openai-agents-python) tool guardrail (Python) |
| [`jamjet`](https://pypi.org/project/jamjet/) | `pip install jamjet` | Python SDK + runtime |
| [`dev.jamjet:jamjet-spring-boot-starter`](https://central.sonatype.com/artifact/dev.jamjet/jamjet-spring-boot-starter) | Maven / Gradle | [Spring AI](https://spring.io/projects/spring-ai) ChatClient advisors |
| [`@jamjet/cloud`](https://npmjs.com/package/@jamjet/cloud) | `npm i @jamjet/cloud` | TypeScript SDK + shared engine |
| [`@jamjet/cli`](https://npmjs.com/package/@jamjet/cli) | `npm i -g @jamjet/cli` | Unified `jamjet audit show` / `jamjet approve` |

All adapters load the same `policy.yaml`. All emit conformant audit JSONL to `~/.jamjet/audit/`. Run `jamjet audit show` to tail every decision across every adapter in one chronological view.

JamJet does not replace the hook points these platforms give you. It makes them do more: Claude Code's PreToolUse hook gets a real policy engine, approval flow, and audit trail, and the same rules carry unchanged to OpenAI Agents SDK, MCP clients, Spring AI, and your own Python or TypeScript code.

### One policy, every adapter

```yaml
# ~/.jamjet/policy.yaml
version: 1
rules:
  - { match: "*delete*", action: block }
  - { match: "payments.*", action: require_approval }
  - { match: "shell.exec", action: block }
```

Drop this file in `~/.jamjet/`. Every adapter listed above uses it automatically.

> Prompts are not a security boundary. The runtime is.

→ Read **[When AI Deletes the Database](https://jamjet.dev/blog/when-ai-deletes-the-database/)** for why this is a runtime architecture problem, not a model problem.

## Policy in your own code

Drop a policy beside your agent code. The runtime checks every step against it before a worker runs the step: a `blocked_tools` match fails the step, and a `require_approval_for` match holds it until a reviewer decides. For a Python step the patterns match its handler (`module:function`, or the same path with dots) and its node id. The request and the decision are stored, so a restart loses neither. Outside dev mode the person who started a run cannot approve it.

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
# Gated in this process: sync OpenAI chat.completions create/parse/stream and
# Anthropic messages and beta.messages create/parse: *delete* tools are
# stripped, and a database.* tool call waits for approval. OpenAI's Responses
# API, Anthropic's messages.stream(), async clients, batch APIs and raw
# streamed responses are not enforced and refuse to run while a block or
# approval rule is set.
```

→ Runnable approval workflow in **[`examples/hitl-approval`](examples/hitl-approval)** · [Cloud Quickstart](https://docs.jamjet.dev/en/docs/cloud-quickstart)

## Where JamJet sits

```text
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
| Costs are discovered after the bill | Token and dollar caps per run, counted from the run's own log |
| A retried job starts the run a second time | The same `run_key` returns the run the first start made |
| Audit evidence is stitched from logs | Append-only event log per run; approvals sealed into a signed, hash-chained audit log |
| Memory is framework-specific | Pair with [Engram](#engram) for portable memory (MCP · REST · Python · Java) |
| Frameworks stay siloed | One policy file across hooks, MCP clients and SDKs; the runtime is also an MCP server (`/mcp`) |

## A full runtime underneath

The safety layer runs on a durable, event-sourced execution engine. When you want more than enforcement, it's already there:

- **Durable execution.** Event log, snapshots and crash recovery: after a restart a run picks up where it stopped, and steps that already completed do not run again. `agent.run_durable(...)` in [`examples/react-agent-durable`](examples/react-agent-durable).
- **Sessions and memory.** Persistent `Session` threads across runs and restarts, with a governed Engram retrieve/record loop. [`examples/session-memory`](examples/session-memory).
- **Multi-agent.** `Sequential`, `Parallel`, coordinator `Team`, and `Loop`; each sub-agent runs as its own governed durable execution. [`examples/team-multi-agent`](examples/team-multi-agent). Declare whole fleets in YAML with cron schedules: [`examples/fleet`](examples/fleet).
- **Evaluation.** Batch eval with LLM-judge scoring ([`examples/eval-harness`](examples/eval-harness)) and trajectory regression diffs ([`examples/trajectory-eval`](examples/trajectory-eval)).
- **Storage.** SQLite by default, and a Postgres store (preview) for runs, events, the audit log and agents, migrated with `jamjet-server migrate`. [Deployment guide](docs/guides/deployment.md#postgres-preview).
- **Request-driven mode.** With `JAMJET_BACKGROUND=off` the engine runs no loops of its own: each request advances its run, and a scheduler calls `POST /ops/tick` for approval deadlines and lapsed leases. [Request-driven mode](docs/guides/deployment.md#request-driven-mode).
- **Deploy.** `agent.deploy(runtime="local" | "self-host" | "cloud")` ships the same compiled IR to any runtime. [`examples/deploy-an-agent`](examples/deploy-an-agent).
- **Dev loop.** `jamjet create` scaffolds a project; `jamjet dev` runs the whole local stack with one command.
- **Web Companion.** A UI embedded in the runtime binary: graph view, timeline and state inspector.

## Works with your stack, not a replacement

JamJet does not replace LangChain, LangGraph, CrewAI, Google ADK, Spring AI, or your custom agent code. Use those to build agent behavior. Use JamJet to control what happens at runtime.

| You're using | Keep it for | JamJet adds |
|---|---|---|
| **LangChain · LangGraph · CrewAI · Google ADK · AutoGen** | Authoring agent behavior | Runtime safety: policy, audit, replay, approvals |
| **LangSmith · Arize · Weights & Biases** | Observability and evaluation | Active enforcement (block at runtime) + durable recovery |
| **Temporal · Orkes · DBOS** | General durable workflows | Agent-native primitives: policy on tool calls, MCP/A2A, memory |
| **Google · AWS · Azure agent platforms** | Cloud-native ecosystems | Open-source, cloud-neutral governance that works on-prem |

Want to build the official integration for *your* framework? **[Claim a slot](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration)**: 8 slots open, the first 10 merged contributors get JamJet swag.

## Examples

| Example | What it shows |
|---------|--------------|
| [`01-block-unsafe-tool`](examples/01-block-unsafe-tool) | A destructive tool call blocked before execution |
| [`venue-booking-agent`](examples/venue-booking-agent) | The video code-along: one agent in twelve steps, from a first tool to approval, a blocked tool, `kill -9`, a venue reply that gives orders, and tools that refuse a venue they do not know |
| [`hitl-approval`](examples/hitl-approval) | Human approval as a first-class workflow primitive |
| [`react-agent-durable`](examples/react-agent-durable) | A ReAct agent on the durable engine: event log, replay, park-on-429 |
| [`team-multi-agent`](examples/team-multi-agent) | Multi-agent Teams, each sub-agent its own governed run |
| [`loan-underwriter-agent`](examples/loan-underwriter-agent) | Spring Boot agent that survives `kill -9` and gates on human approval |
| [`claims-processing`](examples/claims-processing) | Insurance pipeline: 4 specialist agents + HITL + audit |
| [`eval-harness`](examples/eval-harness) | Batch evaluation with LLM judge scoring |

→ [All 34 examples](examples/)

## Engram

**[Engram](runtime/engram-server/README.md)** is the JamJet ecosystem's memory layer for agents. Where JamJet provides durable *execution* (process can crash and resume), Engram provides durable *memory* (facts persist across runs and version cleanly via `supersede()`). Temporal knowledge graph, hybrid retrieval, conflict detection. Ships as a [Rust crate](https://crates.io/crates/jamjet-engram), an [MCP server](https://registry.modelcontextprotocol.io/v0/servers?search=io.github.jamjet-labs/engram-server) (Docker · GHCR), a [standalone Python library](https://pypi.org/project/jamjet-engram) ([github.com/jamjet-labs/engram](https://github.com/jamjet-labs/engram), 71% on LongMemEval-S), a [Python client](https://pypi.org/project/jamjet) for the MCP server, and a [Spring AI `ChatMemoryRepository`](https://central.sonatype.com/artifact/dev.jamjet/engram-spring-boot-starter). Comparison with Mem0/Zep → [java-ai-memory.dev](https://java-ai-memory.dev).

## Architecture

<details>
<summary><strong>Stack diagram</strong></summary>

```
┌──────────────────────────────────────────────────────────┐
│                     Authoring Layer                      │
│    Python SDK  |  Java SDK  |  TypeScript SDK  |  YAML   │
├──────────────────────────────────────────────────────────┤
│                 Compilation / Validation                 │
│           Graph IR  |  Schema  |  Policy lint            │
├────────────────────────────┬─────────────────────────────┤
│      Rust Runtime Core     │      Protocol Layer         │
│  Scheduler  |  State SM    │  MCP server (/mcp)          │
│  Event log  |  Snapshots   │  Agent discovery            │
│  Workers    |  Timers      │                             │
├────────────────────────────┴─────────────────────────────┤
│                        Governance                        │
│    Policy  |  Approvals  |  RBAC  |  Signed audit log    │
├──────────────────────────────────────────────────────────┤
│                     Runtime Services                     │
│    Model adapters  |  Model sidecar  |  External workers │
├──────────────────────────────────────────────────────────┤
│                         Storage                          │
│ SQLite  |  Postgres (preview)  |  In-memory (ephemeral)  │
└──────────────────────────────────────────────────────────┘
```

</details>

## Documentation

Full docs at **[docs.jamjet.dev](https://docs.jamjet.dev/en/docs/open-source/quickstart)**. The guides in [`docs/guides`](docs/guides) are versioned with the code:

[Quickstart](docs/guides/quickstart.md) · [Concepts](docs/guides/concepts.md) · [Python SDK](docs/guides/python-sdk.md) · [Java SDK](docs/guides/java-sdk.md) · [YAML Reference](docs/guides/yaml-reference.md) · [Human-in-the-Loop](docs/guides/hitl.md) · [Security](docs/guides/security.md) · [Deployment](docs/guides/deployment.md) · [Observability](docs/guides/observability.md) · [MCP](docs/guides/mcp-guide.md) · [A2A](docs/guides/a2a-guide.md) · [Eval](docs/guides/eval.md) · [REST API](https://docs.jamjet.dev/en/docs/reference/rest-api) · [CLI](https://docs.jamjet.dev/en/docs/open-source/cli)

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

**Looking for a starter task?**
- Build a [framework integration](https://github.com/jamjet-labs/jamjet-examples/issues?q=is%3Aissue+is%3Aopen+label%3Awanted-integration): 8 slots open, first 10 contributors get JamJet swag
- Browse [good first issues](https://github.com/jamjet-labs/jamjet/labels/good%20first%20issue)
- Join the conversation in [Discord](https://discord.gg/SAYnEj86fr)

## Community

[GitHub Discussions](https://github.com/jamjet-labs/jamjet/discussions) · [Issues](https://github.com/jamjet-labs/jamjet/issues) · [Discord](https://discord.gg/SAYnEj86fr)

## License

Apache 2.0. See [LICENSE](LICENSE).

---

<div align="center">

*Hosted control plane available at [app.jamjet.dev](https://app.jamjet.dev): traces, approval queue, audit retention, team projects. Optional. The runtime, all SDKs, and Engram are Apache-2.0 with no usage limits.*

### ⭐ Star JamJet if you believe agents need a runtime safety layer

<sub>Built by <a href="https://github.com/sunilp">Sunil Prakash</a> · © 2026 JamJet Labs · <a href="https://jamjet.dev">jamjet.dev</a> · Apache 2.0</sub>

</div>
