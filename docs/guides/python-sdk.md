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
`get_agent`, `find_execution`, `require_features`, `claim_for_execution` (the
engine gives the same worker the lease it already holds), a heartbeat,
`start_execution` with a `run_key` (the engine answers a second send with the
first run), and a completion or failure that carries its lease fence (the
engine refuses a second one with 409). Not safe: `start_execution` with no
`run_key` or with `reuse="failed_only"`, `cancel_execution`, `approve`,
`claim_work_item`, `create_cron_job`, `register_agent`, `activate_agent`,
`deactivate_agent`, `discover_agent`, `send_external_event`, and a completion
or failure with no fence. Anything not on the first list is treated as not
safe.

---

## Driving a run from your own process

`run_durable` starts a run and then waits. Something else works its steps: a
`jamjet worker` runs the tool steps, and the engine runs the model steps.
Drive mode puts all of that in the process that started the run. Its tool
steps run through the agent's `@tool` functions, and its model steps run
through a model you pass in, which can send the call on to your own router.
Use it where you cannot run a separate worker, such as a job on a serverless
container platform with an engine behind it.

```python
from jamjet import Agent, tool
from jamjet.model import Model, ModelRequest, ModelResponse

@tool
def find_venues(city: str) -> str:
    ...

class MyRouterBackend:
    async def complete(self, request: ModelRequest) -> ModelResponse:
        step = request.metadata["jamjet"]
        answer = await my_router.complete(request.messages, key=step["idempotency_key"])
        return ModelResponse(
            message={"content": answer.text, "tool_calls": answer.tool_calls},
            input_tokens=answer.input_tokens,
            output_tokens=answer.output_tokens,
            cost_usd=answer.cost_usd,  # None when you do not know it
            usage_reported=True,
            model=answer.model,
        )

    def stream(self, request: ModelRequest):
        raise NotImplementedError("drive mode does not stream")

agent = Agent("planner", model="openai/gpt-4o", tools=[find_venues], instructions="Find a venue.")

result = await agent.run_durable(
    "Find a venue for 40 people on Friday",
    runtime_url="https://engine.example",
    run_key="booking:2026-11-27",
    drive=True,
    model=Model(backend=MyRouterBackend()),
)
```

The engine has to list `python_fn_by_execution` and `worker_model_steps` in
`/health` (and `run_key`, when you pass one). If it does not, `run_durable`
raises `EngineFeatureMissing` before it registers or starts anything. Update
the engine first.

The call returns what `run_durable` always returns, and the run is an ordinary
run: `jamjet executions`, `jamjet audit` and the approval commands work on it.
The graph it registers differs from the one `run_durable` registers without
`drive=True`: every model step is handed to the run's own worker, and every
tool step can be claimed only by the token that started the run. It has its
own version, so the same agent can have both on one engine. `jamjet worker`
never gets these steps.

### `run_key`: the same run after a retry

A `run_key` names the run. Start again with the same key, the same agent, the
same prompt and the same `max_turns`, and you get the run the first start
made, not a second one.
That is how a job that was killed finds its run again. The key is 1 to 200
characters of `A-Z a-z 0-9 . _ : / @ + = -`.

- The same key with another prompt or another agent raises `RunKeyConflict`
  (import it from `jamjet.transport`). Its `existing_execution_id` names the
  run the key holds. Keep the prompt the same across retries. A prompt that
  carries a time or a random value will not find its run.
- A key keeps its run for good. Once the run has ended, the same key returns
  the ended run. To run the work again, use a new key, for example one that
  ends in an attempt number.
- Without a key, a start that lost its answer raises `OutcomeUnknown` and is
  not sent again, because a second send could start a second run. The message
  says to pass a key. For the same reason, a start with `reuse="failed_only"`
  is not sent again after a lost answer. Look the run up with
  `client.find_execution(run_key)`.
- A start through `run_durable` has no `reuse` argument. The key and its default
  rule are what it uses.

### `model=`: the caller's model

`model` is a `jamjet.model.Model`, the seam with its middleware, so your
allow list, PII redaction and budget still apply. Without it, drive mode uses
the agent's own model with the agent's governance. A `model` that is not a
`Model` is a `TypeError` before anything is sent, and so is `model=` without
`drive=True`.

The request is the one the engine built from the run: the node's system
prompt, then its prompt, or the run's conversation when the node has no
prompt. Your backend sees the run on `request.metadata["jamjet"]`:
`execution_id`, `node_id`, `item_id`, `attempt`, `idempotency_key`, `run_key`
and `budget`, which is what the run's caps leave when the step was claimed
(`None` when the run has none). When the node has an output schema it is on
`request.metadata["output_schema"]`.

What the backend's `ModelResponse` must carry:

- `model`: the name of the model that answered. The engine holds it to the
  run's allow list. The LiteLLM backend reports the model it was asked for,
  because the provider's own name is often a dated snapshot that an allow list
  does not hold. A response with no name fails the step.
- `usage_reported=True`, with `input_tokens` and `output_tokens` read from the
  call. It defaults to false, so a backend that leaves the counts at 0 has
  reported nothing and is not read as free.
- `cost_usd`: the cost if you know it, else `None`. Never 0 for an unknown cost.
  A cost that is not a number from 0 fails the step. With a dollar cap on the
  run, a step with no cost fails the run, since the engine cannot count it.
- `message` with the content and the tool calls, as an OpenAI message or a dict
  with `content` and `tool_calls`. The finish reason is set for you: `tool_calls`
  when there are calls, else the message's own, else `stop`.

A backend that reports no usage cannot complete a step. The engine refuses an
answer with no token counts, so the step fails and is tried again like any
failed step, and goes to the dead-letter queue when its attempts run out.
A response that fails any of these checks is reported to the engine as the
step's failure, with the reason.

The engine counts usage when it records an answer. A call that was paid for and
then failed, or whose answer arrived after the lease ran out, is not in the
run's caps. Keep your own record of spend, keyed by the idempotency key.
A budget that lives in the caller's middleware counts in this process only, and
starts again from zero in a process that is started again. The caps in the run
count across restarts.

### What a run does when it ends

| The run | `run_durable` | A job should |
|---------|---------------|--------------|
| completed | returns the `AgentResult` | exit 0 |
| failed, or stopped by a limit | raises `RuntimeError` ("ended in non-completed state"), with `execution_id` set | exit 1 |
| cancelled | raises `RunCancelled` | exit 1 |
| waiting for an approval | raises `RunWaiting`, `waiting_for == "approval"` | exit 0, and start again with the same `run_key` after the decision |
| its step is held by another worker, and nothing moves for 180 s | raises `RunWaiting`, `waiting_for == "worker"` | exit 0, and start again later |
| no progress for 180 s | raises `RunStalled` | exit 1 |
| the engine will not hand out the next step (`blocked`, `refused`), or this token did not start the run (`not_found`) | raises `RunBlocked`, with `reason` | exit 1 |
| the engine answers 401 or 403 | raises `JamjetAPIError` (`PlatformAuthError` when the answer is not the engine's), nothing more is sent | exit 1 |

Import `RunWaiting`, `RunStalled`, `RunBlocked`, `RunCancelled` and
`RunInterrupted` from `jamjet.drive`. Each carries `execution_id`.

`RunWaiting` is not a failure. A job that drives a run exits 0 on it and is
started again later, with the same `run_key`, and goes on where the run
stopped. A stall names an overdue tick when the engine reports one: with its
loops off, nothing runs the engine's own steps until something calls
`POST /ops/tick`.

The call has no time limit of its own. It ends by the table above. Each model
call is cut after the node's own `timeout_secs`, or after 600 s when it has
none. To stop a drive from outside, call `jamjet.drive.drive_run` yourself
with a `stop` event: nothing more is claimed, the step in hand gets 5 s, and
`RunInterrupted` is raised. `run_durable` installs no SIGTERM handler. A job
that is killed leaves its step's lease to lapse, and the run goes on when the
job is started again with the same key.

### At least once: what a step can do twice

A step can run more than once. A step whose lease runs out, because the
process stalled or lost the engine for longer than the lease, is handed out
again. A process that is killed and started again
gets a new worker id, so it waits up to one lease before the step is handed
out again. A step that fails is tried again by the engine's retry policy. A
step that has lost its lease keeps running until it ends, and its result is
refused.

Every attempt of a step carries the same idempotency key. A tool gets it from
the work context:

```python
from jamjet.worker_context import current_work

@tool
def book_room(venue: str) -> str:
    key = current_work().idempotency_key   # the same on every attempt
    return reserve(venue, idempotency_key=key)
```

A tool that writes outside JamJet has to pass the key on, or check it, so a
repeat does nothing. `current_work().run_key` is the run's key. A model step
carries the key on `request.metadata["jamjet"]["idempotency_key"]`. A router
that keeps its answers by that key can answer a repeat without a second charge.
The LiteLLM backend does not use the key: it calls the provider again, and the
provider charges again. The engine records one answer for a step, and a second
one is refused with 409.

Within one process, a run is driven once at a time. A second call for a run the
process is already driving raises `RunWaiting` at once and claims nothing. A
second process on the same run waits for the first one's step like any other
worker.

### Limits

- One token may hold 16 waiting claims on an engine process. Drive no more
  than 16 runs at once with one token. A claim past that, with nothing to hand
  out, is answered 429. The client waits as `Retry-After` says, sends it
  again, and then raises.
- The tool functions must import in the driving process under the name the
  run was started with (`module:function`). A tool defined in the script that
  starts the run works while that script is the one that drives it.
- If the engine cannot be reached, the call raises the transport's error. Start
  it again with the same `run_key`.
- An older engine raises `EngineFeatureMissing` before anything is written.
  An older SDK on this engine works as before: `jamjet worker` and a plain
  `run_durable` do not change.

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
