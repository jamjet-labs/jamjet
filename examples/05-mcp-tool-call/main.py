"""
Example 5: enforce JamJet policy before an MCP tool call
========================================================

Runs a small JamJet workflow where one step evaluates policy before calling a
local MCP server over stdio. JamJet blocks a dangerous-looking MCP tool before
execution, then allows and calls the safe `add` tool. The second step uses a
deterministic mocked model summary.

Run:
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
from pydantic import BaseModel, Field

from jamjet import Workflow
from jamjet.cloud.policy import PolicyEvaluator


def env_int(name: str, default: int) -> int:
    """Read an integer environment variable with a clear validation error."""
    value = os.getenv(name)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError as exc:
        raise ValueError(f"{name} must be an integer, got {value!r}") from exc


def run_mcp_server() -> None:
    """Run the local calculator MCP server over stdio."""
    server = FastMCP("jamjet-local-calculator")

    @server.tool()
    def add(a: int, b: int) -> int:
        """Add two integers."""
        return a + b

    @server.tool()
    def delete_history() -> str:
        """A dangerous-looking demo tool that should be blocked by policy."""
        raise RuntimeError("delete_history should be blocked by policy before execution")

    server.run(transport="stdio")


class State(BaseModel):
    """State passed between the MCP tool call and summary steps."""

    a: int
    b: int
    available_tools: list[str] = Field(default_factory=list)
    allowed_tool: str = "add"
    blocked_tool: str = "delete_history"
    allowed_policy_result: str = ""
    blocked_policy_result: str = ""
    allowed_policy_rule: str | None = None
    blocked_policy_rule: str | None = None
    mcp_result: int | None = None
    summary: str = ""


wf = Workflow("mcp-tool-call")


@wf.state
class McpToolState(State):
    """Typed JamJet workflow state for the MCP tool call example."""

    pass


@wf.step
async def enforce_policy_and_call_mcp_tool(state: McpToolState) -> McpToolState:
    """Evaluate JamJet policy before calling the allowed MCP tool."""
    evaluator = PolicyEvaluator()
    evaluator.add("block", "*delete*")
    evaluator.add("allow", "add")

    blocked_decision, blocked_allowed = evaluate_tool(evaluator, state.blocked_tool)
    if blocked_allowed:
        raise RuntimeError(f"Expected {state.blocked_tool} to be blocked by policy")

    allowed_decision, add_allowed = evaluate_tool(evaluator, state.allowed_tool)
    if not add_allowed:
        raise RuntimeError(f"Tool blocked by policy: rule {allowed_decision.pattern!r}")

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
                tool_names = sorted(tool.name for tool in tools.tools)
                require_tools(tool_names, state.allowed_tool, state.blocked_tool)

                result = await session.call_tool(
                    state.allowed_tool,
                    arguments={"a": state.a, "b": state.b},
                )
                value = extract_tool_result(result)

    return state.model_copy(
        update={
            "available_tools": tool_names,
            "allowed_policy_result": format_policy_result(allowed_decision),
            "blocked_policy_result": format_policy_result(blocked_decision),
            "allowed_policy_rule": allowed_decision.pattern,
            "blocked_policy_rule": blocked_decision.pattern,
            "mcp_result": value,
        }
    )


@wf.step
async def summarize_with_mock_model(state: McpToolState) -> McpToolState:
    """Summarize the policy and MCP result with a deterministic mocked model."""
    if state.mcp_result is None:
        raise ValueError("MCP tool did not return a result")

    summary = (
        f"JamJet blocked {state.blocked_tool} before execution, then allowed "
        f"{state.allowed_tool} and received {state.mcp_result}."
    )
    return state.model_copy(update={"summary": summary})


def evaluate_tool(evaluator: PolicyEvaluator, tool_name: str) -> tuple[Any, bool]:
    """Return the policy decision plus whether the tool may execute."""
    decision = evaluator.evaluate(tool_name)
    return decision, not decision.blocked


def format_policy_result(decision: Any) -> str:
    """Format a policy decision for terminal output."""
    return "BLOCKED" if decision.blocked else "ALLOWED"


def require_tools(tool_names: list[str], *required_tools: str) -> None:
    """Fail early if the MCP server does not advertise the expected tools."""
    missing = [tool for tool in required_tools if tool not in tool_names]
    if missing:
        raise RuntimeError(f"MCP server did not advertise expected tools: {', '.join(missing)}")


def extract_tool_result(result: Any) -> int:
    """Extract the integer result from an MCP tool response."""
    structured = getattr(result, "structuredContent", None)
    if structured is None:
        structured = getattr(result, "structured_content", None)
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
        return f"{step:<34} {status:<10} {duration_us / 1000:>7.1f}ms"

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

    print()
    print("Model     : mocked")
    print("MCP server: local stdio calculator")
    print("Policy    : block '*delete*', allow 'add'")
    print(f"Tool call : add(a={add_a}, b={add_b})")

    result = wf.run_sync(McpToolState(a=add_a, b=add_b))
    state = result.state

    print("\nPolicy decisions")
    print(
        f"  {state.blocked_tool:<14}: {state.blocked_policy_result} "
        f"by rule {state.blocked_policy_rule!r}"
    )
    print(
        f"  {state.allowed_tool:<14}: {state.allowed_policy_result} "
        f"by rule {state.allowed_policy_rule!r}"
    )

    print("\nMCP result")
    print(f"  Available tools: {', '.join(state.available_tools)}")
    print(f"  add returned   : {state.mcp_result}")

    print("\nMock model summary")
    print(f"  {state.summary}")

    print("\nExecution timeline")
    for event in result.events:
        print(f"  {format_event(event)}")
    print(f"\nTotal: {result.total_duration_us / 1000:.1f}ms ({result.steps_executed} steps)")
    print("The model is mocked. The enforcement path is real.")
    print()


if __name__ == "__main__":
    if "--mcp-server" in sys.argv[1:]:
        run_mcp_server()
    else:
        main()
