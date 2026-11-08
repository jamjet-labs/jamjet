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
- 📒 **Record an audit trail** of every decision, with approvals signed and hash-chained
- ⏪ **Replay or resume** crashed runs from the last checkpoint

**Keep your agent framework. Add JamJet where tool calls need control.**

---

## See it in 60 seconds

```bash
pip install jamjet
jamjet demo unsafe-tool-call
```

No API key. No Docker. No cloud account. The model is mocked; the policy check is the SDK's own evaluator. The demos run in process and do not start the runtime. Three more demos run the same way:

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
- **`@durable`** keeps a tool call that completed from running again after a crash or restart, with shims for LangChain, CrewAI, ADK, OpenAI Agents SDK, Anthropic Agent SDK
- **`jamjet.cloud`** — optional two-line install for the hosted control plane (free tier: 5K traces/mo, single project)
- **`jamjet.integrations.openai_guardrail`** — drop-in guardrail for the OpenAI Agents SDK that runs the same `policy.yaml`
- **The runtime's MCP server** (`/mcp`) starts, inspects and approves runs, and `jamjet mcp connect` lists any MCP server's tools

---

## Quick Start

**Requirements:** Python 3.11+ (the `memory` extra, which adds Engram, needs 3.12+)

```bash
# Install the CLI, with the model client and the model sidecar
pip install 'jamjet[model,sidecar]'

# Option A: scaffold a new project
jamjet init my-agent-project
cd my-agent-project

# Option B: add JamJet to an existing project (works like git init)
cd my-existing-project
jamjet init

# Start the local dev stack (SQLite, no server setup needed).
# The starter workflow calls claude-haiku-4-5, so export a key first.
export ANTHROPIC_API_KEY=sk-ant-...
jamjet dev
```

`jamjet dev` uses the `jamjet-server` on your `PATH`, or downloads the one
published with the latest SDK release. The
[Quickstart](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/quickstart.md)
says which runtime it needs and how to build one.

In another terminal, from the project directory:

```bash
# Check, then run, the workflow the template created (it reads `query`)
jamjet validate workflow.yaml
jamjet run workflow.yaml --input '{"query": "What is JamJet?"}'
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

### Hello World: YAML, with a human in the loop

```yaml
# workflow.yaml
workflow:
  id: refund-reply
  version: 0.1.0
  start: draft

nodes:
  draft:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: "Draft a reply to: {{ state.request }}"
    next: review

  review:
    type: human_approval
    description: Approve the draft before it is sent
    timeout: 1h
    fallback: end        # nobody decided in time: stop, do not send
    next: send

  send:
    type: model
    model: claude-haiku-4-5-20251001
    prompt: "Send this: {{ state.last_model_output }}"
    next: end
```

```bash
jamjet validate workflow.yaml
jamjet run workflow.yaml --input '{"request": "refund order 42"}'

# In another terminal, while the run waits at `review`:
jamjet approvals <exec-id>     # what is waiting, and the exact approve command
```

The run stops at `review` until someone decides. The decision is recorded in
the run's state at `state.__approvals.review` (approved, approver, comment,
time), which only the engine writes. If nobody decides within the hour, the
approval expires, `send` is skipped and the run ends. Outside dev mode the
person who started the run cannot approve it.

When `jamjet run` follows a text-mode execution, it also prints the recorded
terminal step results with known durations, proportional bars, and a clearly
labelled recorded step-time total. Missing step timings are shown as unknown;
JSON output keeps its existing event payload unchanged.

### MCP and A2A

A workflow cannot call MCP tools or A2A agents: registration refuses
`mcp_tool` and `a2a_task` nodes. The runtime itself is an MCP server at
`/mcp`. See the
[MCP guide](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/mcp-guide.md)
and the
[A2A guide](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/a2a-guide.md).

---

## Durability across frameworks

Wrap a side-effecting tool with `@durable` and its result is cached under a
key built from the run id, the function and its arguments. After a crash or
restart, a call that already completed returns the cached result instead of
running again, whichever agent framework you use. A call that crashed before
its result was stored runs again, so pass an idempotency key to the external
system as well.

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
    # If the process crashes after `charge_card` returns and restarts under
    # the same AGENT_RUN_ID, the cached result is returned on replay and
    # Stripe is not called again.
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
jamjet.policy("block", "payments.*")              # strip matching tools, refuse matching tool calls
jamjet.budget(max_cost_usd=5.00)                   # cap spend; raises BudgetExceeded
approval = jamjet.require_approval(action="charge_card", context={...})
@jamjet.trace                                       # decorate any function
def lookup_customer(...): ...
```

Policy and budget apply to the sync clients: OpenAI `chat.completions.create`,
`.parse()`, `.stream()` and `with_raw_response`, and Anthropic `messages` and
`beta.messages` `create` / `parse`, legacy OpenAI `functions` included.
Blocked tools are stripped from the request, and a tool call the model returns
for a blocked tool raises `JamJetPolicyBlocked`. With `stream=True`, text
streams through as it arrives; once any policy rule is set, chunks from the
first tool call onward are held until the stream ends and checked before your
code sees them, and the streamed usage counts toward the budget. Server tools
(such as web search) and MCP toolsets run on the vendor's side before a tool
call could be refused, so a rule that could match them removes them from the
request; Anthropic's own client tools (bash, text editor, computer, memory)
are gated like any other tool. Not enforced, so they raise
`JamJetUnenforceableCall` while any block or require_approval rule is set:
OpenAI's Responses API, Anthropic's `messages.stream()`, the async clients
(`AsyncOpenAI`, `AsyncAnthropic`), the batch APIs, and
`with_raw_response` / `with_streaming_response` with `stream=True`.

`jamjet.policy("require_approval", "payments.*")` keeps the tool available.
When the model calls it, the SDK opens an approval in JamJet Cloud with the
tool name, call id, arguments, model and rule, and waits for a reviewer's
decision before returning the response. A rejection raises
`JamJetApprovalRejected`. No decision within `approval_timeout_seconds`
(a `configure()` argument, default 1 hour), or no way to reach Cloud, raises
`JamJetApprovalTimeout`. The timeout is sent to Cloud as the approval's
deadline (clamped to 1 minute to 24 hours); JamJet Cloud deployments since
jamjet-cloud #87 refuse decisions after it. The approval shows the call's
arguments with credentials always masked, PII masked when redaction is on
(IBANs and card numbers keep their last 4), and an `arguments_sha256` that the
SDK re-checks before releasing the approved call.

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
│                     Authoring Layer                      │
│                Python SDK  |  YAML  |  CLI               │
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
│             SQLite  |  In-memory (ephemeral)             │
└──────────────────────────────────────────────────────────┘
```

Read more: [Architecture Overview](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/overview.md)

---

## Documentation

### Start here

| Guide | Description |
|-------|-------------|
| [Quickstart](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/quickstart.md) | Get a workflow running in 10 minutes |
| [Core Concepts](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/concepts.md) | Agents, workflows, nodes, state, durability |
| [Workflow Authoring](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/workflow-authoring.md) | YAML and Python authoring guide |
| [Python SDK](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/python-sdk.md) | Full SDK reference |
| [YAML Reference](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/yaml-reference.md) | Complete YAML spec |

### Connect to other systems

| Guide | Description |
|-------|-------------|
| [MCP Integration](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/mcp-guide.md) | The runtime's MCP endpoint, and listing a server's tools |
| [A2A Integration](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/a2a-guide.md) | Reading and registering Agent Cards |
| [Agent Model](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/agent-model-guide.md) | Agent Cards, lifecycle, autonomy levels |
| [Human-in-the-Loop](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/hitl.md) | Approval nodes, deadlines, separation of duties |
| [Observability](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/observability.md) | Event timelines, replay, OpenTelemetry, token counts |
| [Deployment](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/deployment.md) | Running the server: SQLite, tokens, workers, Docker |

### Advanced & enterprise

| Guide | Description |
|-------|-------------|
| [Security](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/security.md) | Tokens, RBAC, tenant policy, secrets, signed audit log |
| [Tool Sandboxing](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/wasi-sandboxing.md) | What runs where, and how to contain workers |
| [Eval Node](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/eval.md) | Evaluation as a workflow primitive |
| [ANP Discovery](https://github.com/jamjet-labs/jamjet/blob/main/docs/guides/anp.md) | Decentralized agent discovery via DID |

### Architecture deep-dives

| Document | Description |
|----------|-------------|
| [Architecture Overview](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/overview.md) | Full system architecture |
| [Execution Model](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/execution-model.md) | State machine, node types, IR |
| [State & Durability](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/state-and-durability.md) | Event sourcing, snapshotting, recovery |
| [Agent Model](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/agent-model.md) | Agent-native runtime design |
| [MCP Architecture](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/mcp-integration.md) | MCP client/server internals |
| [A2A Architecture](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/a2a-integration.md) | A2A protocol internals |
| [Protocol Adapters](https://github.com/jamjet-labs/jamjet/blob/main/docs/architecture/protocol-adapters.md) | Extensible protocol layer |

---

## Contributing

We welcome contributions of all kinds — bug reports, feature requests, documentation, and code.

- Read [CONTRIBUTING.md](https://github.com/jamjet-labs/jamjet/blob/main/CONTRIBUTING.md) to get started
- Check open issues for `good first issue` tags
- RFCs for major changes: see [docs/rfcs/](https://github.com/jamjet-labs/jamjet/blob/main/docs/rfcs/)
- Architecture decisions: see [docs/adr/](https://github.com/jamjet-labs/jamjet/blob/main/docs/adr/)

---

## Community

- **GitHub Discussions:** [github.com/jamjet-labs/jamjet/discussions](https://github.com/jamjet-labs/jamjet/discussions)


---

## License

Apache 2.0. See [LICENSE](https://github.com/jamjet-labs/jamjet/blob/main/LICENSE).
