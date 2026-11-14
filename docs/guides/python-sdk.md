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
