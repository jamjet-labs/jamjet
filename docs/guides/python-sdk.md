# Python SDK Reference

The JamJet Python SDK is the primary authoring layer. It lets you define agents, tools, schemas, and workflows in Python, which compile to the same runtime IR as YAML definitions.

---

## Installation

```bash
pip install jamjet
```

---

## Core imports

```python
from jamjet import Workflow, tool, agent
from jamjet.models import model_node
from pydantic import BaseModel
```

---

## Defining tools

Tools are Python async functions decorated with `@tool`. JamJet infers input and output schemas from type annotations.

```python
from jamjet import tool
from pydantic import BaseModel

class SearchResult(BaseModel):
    answer: str
    sources: list[str]
    confidence: float

@tool
async def web_search(query: str) -> SearchResult:
    """Search the web and return a structured result."""
    # your implementation
    ...
```

**Rules:**
- Must be `async`
- Input parameters must be type-annotated
- Return type must be a Pydantic `BaseModel` or a primitive (`str`, `int`, `bool`, `list`, `dict`)
- Tools are automatically registered by name and available in YAML via `tool_ref`

---

## Defining workflow state

```python
from pydantic import BaseModel

class ResearchState(BaseModel):
    question: str
    search_result: SearchResult | None = None
    summary: str | None = None
```

State objects must be Pydantic models. All fields should have defaults (except required inputs) so the runtime can initialize partial state.

---

## Defining workflows

```python
from jamjet import Workflow

workflow = Workflow("research")

@workflow.state
class ResearchState(BaseModel):
    question: str
    result: SearchResult | None = None
    summary: str | None = None

@workflow.step
async def search(state: ResearchState) -> ResearchState:
    result = await web_search(query=state.question)
    return state.model_copy(update={"result": result})

@workflow.step
async def summarize(state: ResearchState) -> ResearchState:
    # model call — use jamjet.models.chat() or configure in agents.yaml
    ...
```

**Step rules:**
- Decorated with `@workflow.step`
- Receives the current state, returns updated state
- Steps are executed in definition order unless edges are specified explicitly
- Steps are checkpointed — if the runtime crashes mid-step, the step reruns from the start (idempotency is your responsibility for side effects)

---

## Defining explicit edges

For branching or non-linear workflows, define edges explicitly:

```python
workflow.edge("search", "summarize")
workflow.edge("search", "escalate", condition=lambda s: s.result.confidence < 0.5)
```

---

## Human approval steps

```python
@workflow.step(human_approval=True, timeout="48h")
async def review(state: ResearchState) -> ResearchState:
    # compiled to a human_approval node: the runtime pauses the run here until
    # someone approves or rejects it; it does not run this function
    ...
```

See [Human-in-the-Loop](hitl.md#approval-nodes) for what approve, reject and
expiry do.

---

## Waiting for external events

Not supported by the runtime: it has no executor for `wait` nodes, and
registration refuses them. To wait for a person, use a human approval step.

---

## Retry policies

A step's `retry_policy` is a name. The runtime reads it for the number of
attempts the step gets:

| Name | Attempts |
|------|----------|
| `no_retry` | 1 |
| `io_default` | 5 |
| `llm_default` | 3 |
| any other name, or none | 3 |

```python
@workflow.step(retry_policy="io_default")
async def call_external_api(state: State) -> State:
    ...
```

A failed step is tried again until its attempts run out, waiting 2, 4, 8 and
so on seconds between attempts (at most 64). Some failures end the step at
once: a policy block, or a prompt that does not render. There is no
`RetryPolicy` class, and the backoff is the same for every policy.

---

## Calling an engine: credentials and retries

`JamjetClient` talks to the engine over HTTP. These parts are about what it
sends, where it sends it, and when it sends a call again.

### Credentials go to one address

The JamJet token (`api_token=`, or `JAMJET_TOKEN`) and anything an `auth` hook
adds go only to the client's own origin: the scheme, host and port of
`base_url`. A request to any other address raises `CredentialScopeError`
before anything is sent. Redirects are not followed; a redirect answer is
raised as it is. Error messages and logs never contain a token.

A token with a line break, another control character or a space at one end
cannot go in a header. The client refuses it when it is built, with a
`ValueError` that does not show it, and refuses such a value from an `auth`
hook before the request is sent. A token read from a file often ends with a
line break, so strip it first.

### An engine behind a platform identity check

Some platforms check the caller's identity before a request reaches your
engine. They want an identity token in a header, and the engine wants its own
token in `Authorization`. Pass an `auth` hook for the first and keep
`api_token` for the second:

```python
from jamjet import JamjetClient
from jamjet.auth import GoogleIdTokenAuth

auth = GoogleIdTokenAuth("https://engine.example")
async with JamjetClient("https://engine.example", api_token=jamjet_token, auth=auth) as client:
    await client.get_execution(execution_id)
```

`GoogleIdTokenAuth` needs `pip install 'jamjet[google-auth]'`. It gets an ID
token for the audience from the metadata server or from the service account
file `GOOGLE_APPLICATION_CREDENTIALS` names, and sends it in
`X-Serverless-Authorization`. It keeps the token until about five minutes
before it expires. The token goes over https only (loopback is allowed for
tests), and only to the engine's origin.

For any other token source, use `TokenSourceAuth`:

```python
from jamjet.auth import TokenSourceAuth

auth = TokenSourceAuth(get_token, origin="https://engine.example", header="X-Platform-Token")
```

`get_token` is a function, sync or async, that returns the token string. The
rules are the same:

- A request is never sent without the token. If the source fails, you get
  `IdentityTokenError` and nothing is sent.
- A source that does not answer within the client's `timeout` (or
  `fetch_timeout_s=`, when you pass it) counts as failed. It is tried twice
  more, then the call and every call waiting on the same token get
  `IdentityTokenError`.
- A 401 from the platform gets one fresh token and one resend. A second 401
  raises `PlatformAuthError`.
- A 401 or 403 that does not carry the engine's JSON error body came from
  something in front of the engine. It is raised as `PlatformAuthError`, a
  subclass of `JamjetAPIError`, so existing handlers still catch it.
- The hook cannot write `Authorization` while a JamJet token is set. The
  client refuses that.

The same flag exists on the command line: `jamjet ops tick
--google-id-token-audience https://engine.example`.

### Retries

Retries are on by default: up to 3 sends, a random wait before each (up to
0.2 s, then 0.4 s, capped at 5 s), or the wait an answer's `Retry-After` asks
for, up to 30 s. A longer `Retry-After` is raised at once. Pass
`retry=RetryPolicy(...)` to change the numbers, or `retry=RetryPolicy.off()` to
send every call once.

What is sent again depends on what could have happened:

| What happened | A call that is safe to repeat | Any other call |
|---------------|-------------------------------|----------------|
| The request never left (no connection could be made) | sent again | sent again |
| The engine answered 429 | sent again | sent again |
| The engine answered 5xx | sent again | raises `OutcomeUnknown`, never sent again |
| The answer was lost (timeout, broken connection) | sent again | raises `OutcomeUnknown`, never sent again |
| Any other 4xx, 401 included | raised | raised |

A call that may have taken effect and lost its answer raises
`OutcomeUnknown` (import it from `jamjet.transport`). It is a `RuntimeError`,
not an httpx error, and the original error is on `__cause__`. Look at the
engine before you repeat it. A call whose body can be read only once (a
generator, a file that cannot seek) is sent once.

Safe to repeat: `health`, `tick`, `create_workflow`, `list_cron_jobs`,
`get_execution`, `list_executions`, `get_events`, `list_approvals`,
`list_pending_approvals`, `put_artifact`, `get_artifact`, `list_agents`,
`get_agent`, a heartbeat, and a
completion or failure that carries its lease fence (the engine refuses a
second one with 409). Not safe: `start_execution`, `cancel_execution`,
`approve`, `claim_work_item`, `create_cron_job`, `register_agent`,
`activate_agent`, `deactivate_agent`, `discover_agent`, `send_external_event`,
and a completion or failure with no fence. Anything not on the first list is
treated as not safe.

---

## Running locally

```bash
jamjet dev          # start local runtime
jamjet run workflow.py --input '{"question": "What is JamJet?"}'
jamjet inspect      # view execution history
```

---

## Next steps

- [YAML Reference](yaml-reference.md) — equivalent YAML authoring
- [Workflow Authoring Guide](workflow-authoring.md) — patterns and best practices
- [MCP Integration](mcp-guide.md) — connecting to external tools
- [A2A Integration](a2a-guide.md) — delegating to external agents
