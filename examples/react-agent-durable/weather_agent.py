"""Tools + agent factory for the durable ReAct example.

This module is imported by BOTH the runner (``main.py``) and the durable Python
tool worker (``jamjet worker --modules weather_agent``). Keeping the ``@tool``
functions and the ``Agent`` factory HERE — not in ``main.py`` / ``__main__`` — is
what makes the durable run actually resolve its tools: the compiled agent-loop IR
records each tool as ``{name: "weather_agent:<fn>"}`` (module + qualname), and the
worker resolves that reference by importing this module. A tool defined in
``__main__`` could never be resolved by the separate worker process.
"""

from __future__ import annotations

from jamjet import Agent, tool


@tool
async def get_weather(city: str) -> str:
    """Return a short current-weather report for a city."""
    table = {
        "paris": "sunny, 24C",
        "london": "cloudy, 17C",
        "tokyo": "rainy, 21C",
        "new york": "windy, 15C",
    }
    return table.get(city.strip().lower(), f"clear skies over {city}, 20C")


@tool
async def add(a: float, b: float) -> str:
    """Add two numbers and return the sum as text."""
    return f"{a + b:g}"


def build_agent() -> Agent:
    """Construct the durable ReAct agent (model + tools + instructions).

    ``strategy="react"`` makes the in-process ``Agent.run()`` use the same
    ``model -> tool -> model`` loop the durable agent-loop IR encodes, so the two
    paths produce the same answer shape (see ``tests/`` parity test in the SDK).
    """
    return Agent(
        "react-weather",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather, add],
        instructions=("You are a concise assistant. Use the tools when they help, then give a short final answer."),
        strategy="react",
    )
