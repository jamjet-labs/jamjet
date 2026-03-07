"""
MCP Tool Provider Example
=========================

Expose JamJet-registered tools as an MCP server so that any MCP-compatible
client (Claude Desktop, VS Code, Cursor, other agents) can discover and invoke
them.

The MCP server runs alongside the JamJet runtime and serves:
  - tools/list   — returns all registered tools
  - tools/call   — invokes a tool and returns the result

Run:
    jamjet dev                       # start the JamJet runtime
    python server.py                 # start the MCP tool provider
    # Then point Claude Desktop / VS Code at: http://localhost:9000/mcp

Tools exposed:
  - calculate     — safe arithmetic expression evaluator
  - web_search    — mock web search (replace with real API)
  - get_weather   — mock weather lookup (replace with real API)
"""

from __future__ import annotations

import asyncio
import math
from typing import Any

from jamjet.tools.decorators import tool
from jamjet.protocols.mcp_server import serve_tools

# ── Tool definitions ──────────────────────────────────────────────────────────


@tool
async def calculate(expression: str) -> dict[str, Any]:
    """
    Evaluate a safe mathematical expression and return the result.

    Supports: +, -, *, /, **, sqrt, sin, cos, tan, log, abs.
    Example: "sqrt(2) + 3 * 4"
    """
    safe_globals = {
        "__builtins__": {},
        "sqrt": math.sqrt, "sin": math.sin, "cos": math.cos,
        "tan": math.tan, "log": math.log, "abs": abs, "pi": math.pi,
    }
    try:
        result = eval(expression, safe_globals)  # noqa: S307 — sandboxed globals
        return {"result": result, "expression": expression}
    except Exception as e:
        return {"error": str(e), "expression": expression}


@tool
async def web_search(query: str, max_results: int = 5) -> dict[str, Any]:
    """
    Search the web and return the top results.

    Args:
        query:       The search query.
        max_results: Maximum number of results to return (1–10).
    """
    # Replace with a real search API (Brave, Serper, Tavily, etc.)
    await asyncio.sleep(0.1)  # simulate network latency
    return {
        "query": query,
        "results": [
            {"title": f"Result {i} for '{query}'", "url": f"https://example.com/{i}"}
            for i in range(1, min(max_results, 10) + 1)
        ],
    }


@tool
async def get_weather(location: str, unit: str = "celsius") -> dict[str, Any]:
    """
    Get current weather for a location.

    Args:
        location: City name or coordinates (e.g. "London" or "51.5,-0.12").
        unit:     Temperature unit: "celsius" or "fahrenheit".
    """
    # Replace with a real weather API (OpenWeatherMap, WeatherAPI, etc.)
    await asyncio.sleep(0.05)
    temp = 18.0 if unit == "celsius" else 64.4
    return {
        "location": location,
        "temperature": temp,
        "unit": unit,
        "condition": "Partly cloudy",
        "humidity_pct": 65,
    }


# ── Server ────────────────────────────────────────────────────────────────────

if __name__ == "__main__":
    import uvicorn

    # serve_tools() builds an Axum-compatible Starlette ASGI app exposing all
    # @tool-decorated functions via the MCP JSON-RPC protocol.
    app = serve_tools(
        tools=[calculate, web_search, get_weather],
        server_name="jamjet-example-tools",
        server_version="0.1.0",
    )

    print("JamJet MCP Tool Provider running at http://localhost:9000/mcp")
    print("Point your MCP client at: http://localhost:9000/mcp")
    uvicorn.run(app, host="0.0.0.0", port=9000)
