"""Compile a sync sentiment-analysis workflow without calling a remote agent."""

from __future__ import annotations

import json
from typing import Any

from jamjet.agent_tool import agent_tool
from jamjet.workflow.graph import WorkflowGraph


def build_workflow() -> dict[str, Any]:
    """Use one request/response to classify a customer-support message."""
    # Each message is independent: no progress stream or conversation is needed.
    classifier = agent_tool(
        agent="https://sentiment.example.com",
        description="Classify the sentiment of one customer-support message.",
        mode="sync",
        timeout_ms=5000,
    )
    graph = WorkflowGraph("support-sentiment")
    graph.add_agent_tool(
        "classify_sentiment",
        agent=classifier.agent_uri,
        mode=classifier.mode,
        timeout_ms=classifier.timeout_ms,
        output_key="sentiment",
        input_mapping={"message": "support_message"},
    )
    return graph.compile()


def main() -> None:
    initial_state = {"support_message": "My replacement arrived quickly. Thank you!"}
    print(f"Sample workflow state: {json.dumps(initial_state)}")
    print("\nCompiled workflow (no agent is invoked):")
    print(json.dumps(build_workflow(), indent=2))


if __name__ == "__main__":
    main()
