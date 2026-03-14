# RFC-004: Python SDK Design

| Field | Value |
|-------|-------|
| RFC | 004 |
| Status | Draft |
| Created | 2026-03-07 |

---

## Summary

Defines the Python SDK authoring API: decorator model, workflow builder, YAML compiler, and CLI.

---

## Key Design Points

### Three Authoring Modes

**1. Decorator API** — for most workflows
```python
@tool
async def web_search(query: str) -> SearchResult: ...

workflow = Workflow("research")

@workflow.state
class ResearchState(BaseModel):
    question: str
    result: SearchResult | None = None

@workflow.step
async def search(state: ResearchState) -> ResearchState: ...
```

**2. Graph Builder API** — for complex orchestration
```python
graph = WorkflowGraph("complex")
graph.add_node("fetch", ToolNode(tool_ref="fetch_data"))
graph.add_node("analyze", ModelNode(model="default_chat"))
graph.add_edge("fetch", "analyze")
```

**3. YAML** — for configuration-first workflows
Parsed by `YamlCompiler` → same IR as Python path.

### Compiler
Both Python and YAML paths compile to identical canonical IR. The compiler validates the graph and schemas before submitting to the runtime API.

### Communication with Runtime
The Python SDK talks to the Rust runtime via REST API (no direct bindings in v1). This keeps the architecture clean and polyglot-ready. Java SDK uses the same pattern; Go and TypeScript SDKs will follow.

### CLI
Built with Typer. Commands: `init`, `dev`, `run`, `validate`, `inspect`, `events`, `agents`, `mcp`, `a2a`.

---

## Implementation Plan
See progress-tracker.md tasks C.1–C.3.
