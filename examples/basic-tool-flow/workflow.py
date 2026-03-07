"""
Basic Tool Flow Example
=======================

A simple JamJet workflow demonstrating:
- Defining a @tool
- Defining workflow state with @workflow.state
- Defining steps with @workflow.step
- Compiling to IR

To run:
    jamjet dev          # start local runtime
    jamjet run workflow.py --input '{"question": "What is JamJet?"}'
"""

from pydantic import BaseModel

from jamjet import Workflow, tool


# ── Tools ────────────────────────────────────────────────────────────────────

class SearchResult(BaseModel):
    answer: str
    confidence: float
    sources: list[str]


@tool
async def web_search(query: str) -> SearchResult:
    """Search the web and return a structured result."""
    # In a real tool, this would call an API.
    return SearchResult(
        answer=f"Result for: {query}",
        confidence=0.9,
        sources=["https://example.com"],
    )


# ── Workflow ──────────────────────────────────────────────────────────────────

workflow = Workflow("basic_tool_flow", version="0.1.0")


@workflow.state
class State(BaseModel):
    question: str
    search_result: SearchResult | None = None
    final_answer: str | None = None


@workflow.step
async def search(state: State) -> State:
    """Search for an answer to the question."""
    result = await web_search(query=state.question)
    return state.model_copy(update={"search_result": result})


@workflow.step(
    model="default_chat",
    next={"end": lambda s: True},
)
async def summarize(state: State) -> State:
    """Summarize the search result into a final answer."""
    # In practice, this would call the LLM via the model adapter.
    if state.search_result:
        answer = f"Based on search: {state.search_result.answer}"
        return state.model_copy(update={"final_answer": answer})
    return state


if __name__ == "__main__":
    import json
    ir = workflow.compile()
    print(json.dumps(ir, indent=2))
