# Workflow Authoring

JamJet has two authoring modes: YAML and Python decorators. Both compile to the same canonical IR.

The runtime executes `model`, `condition` and `human_approval` nodes itself, and Python, Java and `external_fn` steps on external workers.
Registration refuses the other node kinds, including `tool` and `agent` used
in the YAML example below; see [Node types](yaml-reference.md#node-types).

---

## YAML Authoring

### Project structure

`jamjet validate` and `jamjet run` read only the workflow file you pass them.
The other files below show one way to lay out a project; nothing in the CLI or
the runtime loads them.

```
my-project/
  jamjet.yaml         # project config
  workflow.yaml       # workflow graph
  agents.yaml         # agent definitions
  tools.yaml          # tool definitions
  models.yaml         # model configs
  policies.yaml       # policy rules (optional)
  schemas.py          # Pydantic schemas
  prompts/            # prompt templates (.md files)
```

### `jamjet.yaml`
```yaml
project:
  name: customer-support
  runtime_version: 0.1
  default_workflow: triage
```

### `models.yaml`
```yaml
models:
  default_chat:
    provider: openai
    model: gpt-4o
    timeout: 60s
    retry_policy: llm_default
```

### `tools.yaml`
```yaml
tools:
  get_ticket:
    kind: python
    ref: app.tools:get_ticket
    input_schema: schemas.TicketInput
    output_schema: schemas.Ticket
    permissions: [read_only]

  send_email:
    kind: python
    ref: app.tools:send_email
    input_schema: schemas.EmailInput
    output_schema: schemas.EmailResult
    permissions: [write]
```

### `agents.yaml`
```yaml
agents:
  classifier:
    model: default_chat
    system_prompt: prompts/classifier.md
    output_schema: schemas.Classification
    autonomy: guided

  responder:
    model: default_chat
    system_prompt: prompts/responder.md
    output_schema: schemas.Response
```

### `workflow.yaml`
```yaml
workflow:
  id: customer_support_triage
  version: 0.1.0
  state_schema: schemas.TriageState
  start: fetch_ticket

nodes:
  fetch_ticket:
    type: tool
    tool_ref: get_ticket
    input:
      ticket_id: "{{ state.ticket_id }}"
    output_schema: schemas.Ticket
    next: classify

  classify:
    type: agent
    agent_ref: classifier
    next: route

  route:
    type: condition
    conditions:
      - if: 'state.priority == "high"'   # the value is a JSON literal
        next: human_review
    default: auto_respond

  human_review:
    type: human_approval
    timeout: 48h
    next: auto_respond

  auto_respond:
    type: agent
    agent_ref: responder
    next: send_reply

  send_reply:
    type: tool
    tool_ref: send_email
    input:
      to: "{{ state.customer_email }}"
      body: "{{ state.response.body }}"
    next: end
```

---

## Python Decorator API

```python
from jamjet import Workflow, tool
from pydantic import BaseModel

class TriageState(BaseModel):
    ticket_id: str
    ticket: dict | None = None
    classification: dict | None = None
    response: str | None = None

@tool
async def get_ticket(ticket_id: str) -> dict:
    # implementation
    ...

workflow = Workflow("customer_support_triage")

@workflow.state
class State(TriageState): pass

@workflow.step
async def fetch_ticket(state: State) -> State:
    ticket = await get_ticket(ticket_id=state.ticket_id)
    return state.model_copy(update={"ticket": ticket})

@workflow.step(
    next={
        "human_review": lambda s: s.classification.get("priority") == "high",
        "auto_respond": lambda s: True,  # default
    }
)
async def classify(state: State) -> State:
    # model call
    ...

@workflow.step(human_approval=True, timeout="48h")
async def human_review(state: State) -> State:
    # pauses for human
    return state

@workflow.step
async def auto_respond(state: State) -> State:
    # model call
    ...
```

---

## Capping a run's spend

A workflow can cap what one run spends, set at the top level of the workflow (or in the
`workflow:` header):

```yaml
token_budget: { total_tokens: 50000 }   # or input_tokens, output_tokens
cost_budget_usd: 2.50
```

The engine counts a run's tokens and cost from its own event log, across `continue_as_new`
segments, and enforces both caps for in-process and HTTP workers alike. `GET /executions/:id`
shows the count as `usage`: `input_tokens`, `output_tokens`, `cost_usd`, `billable_steps` and
the counts of steps that did not report a cost or tokens. `usage` is null when the log holds
an event this build cannot read, since a partial count would be taken for the whole.

A step is counted when its completion names a model or reports tokens or a cost;
`billable_steps` is the number of counted steps. A step that reports none of these is not
counted at all, under any cap. A `python_fn` that calls a model and returns no usage keys is
never counted, so it is never stopped.

| Cap | A step is let through only if | The run fails when |
|-----|-------------------------------|--------------------|
| `total_tokens` | every counted step reported its tokens, and input plus output is below the cap | a counted step left the count unknown, or input plus output is above the cap |
| `input_tokens`, `output_tokens` | the same, for that count | the same, for that count |
| `cost_budget_usd` | every counted step reported a usable cost, and the cost is below the cap | a counted step left the cost unknown, or the cost is above the cap |

- At the cap, the run goes on. The next step that can spend is refused at claim, and the run
  fails then. Past the cap, the scheduler fails the run in its next pass, before anything
  after the step that passed it is dispatched. The trip is recorded as `token_budget_exceeded`
  or `cost_budget_exceeded`. It names the step that was refused, or, when the scheduler fails
  the run, the last step that spent.
- Steps already running when the cap is reached still finish. In process, all of them are
  recorded and counted, so a run can end above its cap by the steps that were in flight, and
  no more. Parallel branches are the case: two steps claimed under the cap can both run. Over
  HTTP, a step that completes after the run has failed is withdrawn (409), and its spend is
  not recorded.
- The cap is checked on every step that can spend. `condition`, `human_approval` and
  `limit_exceeded` nodes run at the cap, because the engine runs them and they cannot spend.
  An `eval` node is held: an LLM judge spends.
- A counted step that reports no cost under a dollar cap stops the run, and a counted step
  that reports no token count under a token cap does the same. The trip carries `reason`:
  `cost_not_reported` or `tokens_not_reported`. A count that is already over its cap trips as
  a plain overspend. A step that reports a cost but no tokens, `cost_usd: 0` included, counts
  as tokens unknown. A self-reporting worker under a token cap must send both counts, 0
  included.
- The built-in model provider adapters report no cost. The model sidecar does, for a model
  litellm can price; for one it cannot, it sends `cost_usd: null`. Under `cost_budget_usd`, a
  model step through a native adapter, or an unpriced one through the sidecar, stops the run.
- `cost_budget_usd` must be between 0 and 10,000 at registration (400 otherwise). A stored
  value outside that range, from an older build, fails the run on its first pass with the
  trip reason `cost_cap_invalid`.
- The caps are only as good as what a worker reports. A worker that under-reports, or
  reports a failed call as completed, is believed. An `eval` judge and a remote agent called by
  `agent_tool` report nothing, so a step of either kind is held at the cap but its spend is
  never counted.
- A failed attempt's spend is not counted: the fail route takes no usage. A step that fails
  and is retried is counted once, for the attempt that completed. Under a dollar cap, a
  completion refused before the engine reads its cost (a cost sent as a string, or a number
  that does not parse) or sent without its lease fence and with a bad cost leaves the step to
  be retried, and the first attempt's spend is not counted. A malformed cost that reaches the
  handler under a dollar cap fails the step for good instead.
- `on_budget_exceeded` is accepted and read by nothing. `continue_as_new` is no longer read
  for a budget: a run that reaches its cap fails, it does not roll over to a new segment.

---

## Validation

```bash
# Validate YAML workflow
jamjet validate workflow.yaml

# Validate Python workflow
jamjet validate my_workflow.py

# Print the compiled IR
jamjet validate workflow.yaml --output
```
