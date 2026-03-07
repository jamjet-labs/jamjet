# Workflow Authoring

JamJet supports three authoring modes: YAML, Python decorators, and Python graph builder. All three compile to the same canonical IR.

---

## YAML Authoring

### Project structure

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
    next:
      - when: "output.priority == 'high'"
        to: human_review
      - else: auto_respond

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
from jamjet import Workflow, tool, agent
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

## Python Graph Builder API

For complex orchestration where you want explicit control over the graph:

```python
from jamjet import WorkflowGraph, ModelNode, ToolNode, ConditionNode, HumanApprovalNode

graph = WorkflowGraph("complex_pipeline")

graph.add_node("fetch", ToolNode(tool_ref="get_ticket"))
graph.add_node("classify", ModelNode(model="default_chat", prompt="prompts/classify.md"))
graph.add_node("review", HumanApprovalNode(timeout="48h"))
graph.add_node("respond", ModelNode(model="default_chat", prompt="prompts/respond.md"))

graph.add_edge("fetch", "classify")
graph.add_edge("classify", "review", when="output.priority == 'high'")
graph.add_edge("classify", "respond")  # default
graph.add_edge("review", "respond")
```

---

## Validation

```bash
# Validate YAML workflow
jamjet validate workflow.yaml

# Validate Python workflow
jamjet validate my_workflow.py

# Confirm both produce identical IR
jamjet validate workflow.yaml --compare my_workflow.py
```
