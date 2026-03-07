"""
MCP Tool Consumer Example
=========================

Demonstrates connecting to an external MCP server and using its tools inside
a JamJet workflow. JamJet handles connection pooling, retries, and durable
checkpointing automatically.

MCP server options (pick one):
  - stdio:  a local subprocess that speaks the MCP protocol
  - HTTP:   any HTTP+SSE MCP server (e.g. mcp-server-everything, Zapier MCP)

Prerequisites:
    # Install an example MCP server (stdio)
    npx -y @modelcontextprotocol/server-everything

    export ANTHROPIC_API_KEY="sk-ant-..."

Run:
    jamjet dev
    jamjet run workflow.py --input '{"topic": "quantum computing"}'

To test MCP connectivity first:
    jamjet mcp connect stdio:npx,-y,@modelcontextprotocol/server-everything
"""

from pydantic import BaseModel

from jamjet import Workflow


# ── Workflow state ────────────────────────────────────────────────────────────

class State(BaseModel):
    topic: str
    web_results: list[str] = []
    summary: str = ""


# ── Workflow ──────────────────────────────────────────────────────────────────

workflow = Workflow("mcp_tool_consumer", version="0.1.0")
workflow.state(State)

# Register the MCP server once — JamJet manages connection pooling and
# refresh of the tool list when the server updates its capabilities.
workflow.mcp_server(
    name="everything",
    url="stdio:npx,-y,@modelcontextprotocol/server-everything",
)


@workflow.step(
    # This step calls the MCP tool 'brave_search' on the registered server.
    mcp_tool="everything/fetch",
    input_map={"url": "https://en.wikipedia.org/wiki/{topic}"},
    output_map={"content": "web_results"},
)
async def fetch_wiki(state: State) -> State:
    """Fetch Wikipedia content via MCP fetch tool."""
    # The framework injects the MCP tool result into state automatically.
    return state


@workflow.step(
    model="claude-haiku-4-5-20251001",
    next={"end": lambda s: True},
)
async def summarise(state: State) -> State:
    """Summarise the fetched content."""
    content = "\n".join(state.web_results[:5])
    # In practice, the model node sends `content` + `topic` as the prompt.
    state.summary = f"Summary of {state.topic}: {content[:200]}..."
    return state


if __name__ == "__main__":
    import json
    print(json.dumps(workflow.compile(), indent=2))
