"""
Example 5: MCP tool call from a JamJet workflow
===============================================

Runs a small JamJet workflow where one step connects to a local MCP server
over stdio and calls its `add` tool. The second step asks a local Ollama model
to explain the result briefly.

Run:
    export OPENAI_API_KEY="ollama"
    export OPENAI_BASE_URL="http://localhost:11434/v1"
    export MODEL_NAME="llama3.2:3b"
    python main.py

Internal MCP server mode:
    python main.py --mcp-server
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import Any

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.server.fastmcp import FastMCP
from openai import APIConnectionError, OpenAI, OpenAIError
from pydantic import BaseModel, Field

from jamjet import Workflow


def env_int(name: str, default: int) -> int:
    """Read an integer environment variable with a clear validation error."""
    value = os.getenv(name)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError as exc:
        raise ValueError(f"{name} must be an integer, got {value!r}") from exc


MODEL = os.getenv("MODEL_NAME", "llama3.2:3b")


def run_mcp_server() -> None:
    """Run the local calculator MCP server over stdio."""
    server = FastMCP("jamjet-local-calculator")

    @server.tool()
    def add(a: int, b: int) -> int:
        """Add two integers."""
        return a + b

    server.run(transport="stdio")


def llm(system: str, user: str, max_tokens: int = 120) -> str:
    """Call the local Ollama model through its OpenAI-compatible API."""
    client = OpenAI(
        api_key=os.getenv("OPENAI_API_KEY", "ollama"),
        base_url=os.getenv("OPENAI_BASE_URL", "http://localhost:11434/v1"),
    )
    try:
        response = client.chat.completions.create(
            model=MODEL,
            temperature=0,
            max_tokens=max_tokens,
            messages=[
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        )
    except APIConnectionError as exc:
        raise RuntimeError(
            "Could not connect to Ollama. Make sure Ollama is running at "
            "http://localhost:11434 and that the model is pulled, for example: "
            "ollama pull llama3.2:3b"
        ) from exc
    except OpenAIError as exc:
        raise RuntimeError(
            "Ollama returned an error. Make sure the model is available locally, "
            "for example: ollama pull llama3.2:3b"
        ) from exc

    return (response.choices[0].message.content or "").strip()


class State(BaseModel):
    """State passed between the MCP tool call and explanation steps."""

    a: int
    b: int
    available_tools: list[str] = Field(default_factory=list)
    mcp_result: int | None = None
    explanation: str = ""


wf = Workflow("mcp-tool-call")


@wf.state
class McpToolState(State):
    """Typed JamJet workflow state for the MCP tool call example."""

    pass


@wf.step
async def call_mcp_add_tool(state: McpToolState) -> McpToolState:
    """Connect to the local MCP server and call its `add` tool."""
    server_params = StdioServerParameters(
        command=sys.executable,
        args=[str(Path(__file__).resolve()), "--mcp-server"],
        cwd=str(Path(__file__).resolve().parent),
    )

    with open(os.devnull, "w", encoding="utf-8") as errlog:
        async with stdio_client(server_params, errlog=errlog) as (read, write):
            async with ClientSession(read, write) as session:
                await session.initialize()
                tools = await session.list_tools()
                tool_names = [tool.name for tool in tools.tools]

                result = await session.call_tool(
                    "add",
                    arguments={"a": state.a, "b": state.b},
                )
                value = extract_tool_result(result)

    return state.model_copy(
        update={
            "available_tools": tool_names,
            "mcp_result": value,
        }
    )


@wf.step
async def explain_result(state: McpToolState) -> McpToolState:
    """Ask the local model to summarize the MCP tool result."""
    if state.mcp_result is None:
        raise ValueError("MCP tool did not return a result")

    explanation = llm(
        "You explain simple tool results. Keep the answer to one short sentence.",
        f"The MCP tool add received a={state.a} and b={state.b}. It returned {state.mcp_result}.",
    )
    return state.model_copy(update={"explanation": explanation})


def extract_tool_result(result: Any) -> int:
    """Extract the integer result from an MCP tool response."""
    structured = getattr(result, "structuredContent", None)
    if isinstance(structured, dict) and "result" in structured:
        return int(structured["result"])

    for item in getattr(result, "content", []):
        text = getattr(item, "text", None)
        if text is not None:
            return int(str(text).strip())

    raise ValueError(f"Could not extract numeric MCP tool result from: {result}")


def format_event(event: Any) -> str:
    """Format a JamJet execution event without assuming one exact shape."""
    step = getattr(event, "step", None)
    status = getattr(event, "status", None)
    duration_us = getattr(event, "duration_us", None)

    if step is not None and status is not None and duration_us is not None:
        return f"{step:<22} {status:<10} {duration_us / 1000:>7.1f}ms"

    if hasattr(event, "to_dict"):
        try:
            return str(event.to_dict(include_timing=True))
        except TypeError:
            return str(event.to_dict())

    return str(event)


def main() -> None:
    """Run the JamJet workflow and print the MCP result plus timeline."""
    add_a = env_int("ADD_A", 2)
    add_b = env_int("ADD_B", 3)

    print(f"\nModel     : {MODEL}")
    print("MCP server: local stdio calculator")
    print(f"Tool call : add(a={add_a}, b={add_b})\n")

    result = wf.run_sync(McpToolState(a=add_a, b=add_b))
    state = result.state

    print("MCP result")
    print(f"  Available tools: {', '.join(state.available_tools)}")
    print(f"  add returned   : {state.mcp_result}")

    print("\nOllama explanation")
    print(f"  {state.explanation}")

    print("\nExecution timeline")
    for event in result.events:
        print(f"  {format_event(event)}")
    print(
        f"\nTotal: {result.total_duration_us / 1000:.1f}ms ({result.steps_executed} steps)\n"
    )


if __name__ == "__main__":
    if "--mcp-server" in sys.argv[1:]:
        run_mcp_server()
    else:
        main()
