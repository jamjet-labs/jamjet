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
@workflow.step(type="human_approval")
async def review(state: ResearchState) -> ResearchState:
    # execution pauses here until a human approves/rejects via API or CLI
    ...
```

---

## Waiting for external events

```python
@workflow.step(type="wait", event="payment_confirmed", timeout="24h")
async def await_payment(state: OrderState) -> OrderState:
    ...
```

---

## Retry policies

```python
from jamjet import RetryPolicy

@tool(retry_policy=RetryPolicy(max_attempts=3, backoff="exponential", jitter=True))
async def call_external_api(...) -> ...:
    ...
```

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
