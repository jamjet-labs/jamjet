"""Compile a sync sentiment-analysis workflow without calling a remote agent."""

from __future__ import annotations

import json
from typing import Any

from jamjet.agent_tool import agent_tool
from jamjet.workflow.graph import WorkflowGraph


def build_workflow() -> dict[str, Any]:
    """Use one request/response to classify a customer-support message."""
    sentiment = agent_tool(
        agent="https://sentiment.example.com",
        description="Classify a support message as positive, neutral, or negative",
        mode="sync",
        timeout_ms=5000,
    )

    # Each message is independent: no progress stream or conversation is needed.
    graph = WorkflowGraph("support-sentiment")
    graph.add_agent_tool(
        "classify_sentiment",
        agent=sentiment.agent_uri,
        mode=sentiment.mode,
        timeout_ms=sentiment.timeout_ms,
        output_key="sentiment",
    )
    return graph.compile()


def main() -> None:
    print("Sample input: My replacement arrived quickly. Thank you!")
    print("Compiled workflow (no agent is invoked):")
    print(json.dumps(build_workflow(), indent=2))


if __name__ == "__main__":
    main()
