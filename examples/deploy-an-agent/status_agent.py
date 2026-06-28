"""The agent to deploy: a tiny status-reporter (model + one @tool + instructions).

The tools live in this importable module (not in ``__main__``) because the
compiled IR records each tool as ``{name: "status_agent:<fn>"}`` (module +
qualname) and the ``jamjet worker`` resolves it by importing this module. Both
``deploy.py`` and the worker import ``build_agent`` from here.
"""

from __future__ import annotations

from jamjet import Agent, tool


@tool
async def service_health(service: str) -> str:
    """Return a one-line health summary for a named service (demo stub)."""
    return f"{service}: ok (p99 142ms, 0 errors in last 5m)"


def build_agent() -> Agent:
    """Construct the status-reporter agent.

    Governance is on by default (PII redaction, signed audit, receipts). Those
    knobs are compiled into the IR and travel with the deploy unchanged; deploy
    never strips them.
    """
    return Agent(
        "status-reporter",
        model="anthropic/claude-sonnet-4-6",
        tools=[service_health],
        instructions=(
            "You report service health. Call service_health for each service the "
            "user names and summarize the results in one short paragraph."
        ),
    )
