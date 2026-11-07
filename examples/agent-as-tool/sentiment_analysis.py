"""Compile a sync sentiment-analysis workflow without calling a remote agent."""

from __future__ import annotations

import json
from typing import Any

from jamjet.workflow.graph import WorkflowGraph


def build_workflow() -> dict[str, Any]:
    """Use one request/response to classify a customer-support message."""
    # Each message is independent: no progress stream or conversation is needed.
    graph = WorkflowGraph("support-sentiment")
    graph.add_agent_tool(
        "classify_sentiment",
        agent="https://sentiment.example.com",
        mode="sync",
        timeout_ms=5000,
        output_key="sentiment",
        input_mapping={"message": "support_message"},
    )
    return graph.compile()


def main() -> None:
    sample_message = "My replacement arrived quickly. Thank you!"
    print(f"Sample input: {sample_message}")
    print("\nCompiled workflow (no agent is invoked):")
    workflow = build_workflow()
    print(json.dumps(workflow, indent=2))

    # Verify input_mapping is present
    classify_node = workflow["nodes"]["classify_sentiment"]
    print(
        f"\nInput mapping for classify_sentiment node: {classify_node['kind']['input_mapping']}"
    )
    print(
        "The 'support_message' from workflow state will be mapped to 'message' for the agent."
    )


if __name__ == "__main__":
    main()
