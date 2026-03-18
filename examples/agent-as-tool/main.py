"""
Agent-as-Tool — Three invocation modes for research paper processing.

Demonstrates:
- agent_tool() wrapper for wrapping agents as callable tools
- Sync mode: quick classification
- Streaming mode: deep research with progress tracking
- Conversational mode: iterative peer review
- Auto-routing: let the coordinator pick the best agent
"""
from __future__ import annotations

from jamjet.agent_tool import agent_tool
from jamjet.workflow.graph import WorkflowGraph
from jamjet.workflow.nodes import AgentToolNode, ModelNode


def demo_agent_tool_definitions():
    """Show how to define agents as tools with different modes."""
    print("=" * 60)
    print("Agent-as-Tool — Definition Examples")
    print("=" * 60)

    # Sync: quick, stateless — fire and forget
    classifier = agent_tool(
        agent="jamjet://research/classifier",
        mode="sync",
        description="Classifies papers by field, methodology, and contribution type",
        timeout_ms=5000,
    )
    print(f"\n  Classifier (sync):")
    print(f"    URI: {classifier.agent_uri}")
    print(f"    Mode: {classifier.mode}")
    ir = classifier.to_ir_kind()
    print(f"    IR type: {ir['type']}, agent: {ir['agent']}")

    # Streaming: long-running with progress
    researcher = agent_tool(
        agent="jamjet://research/deep-analyst",
        mode="streaming",
        description="Performs deep literature analysis with streamed progress updates",
        budget={"max_cost_usd": 2.00},
        timeout_ms=60000,
    )
    print(f"\n  Researcher (streaming):")
    print(f"    URI: {researcher.agent_uri}")
    print(f"    Mode: {researcher.mode}")
    print(f"    Budget: ${researcher.budget['max_cost_usd']}")

    # Conversational: multi-turn iterative refinement
    reviewer = agent_tool(
        agent="jamjet://research/peer-reviewer",
        mode="conversational",
        description="Iterative peer review — provides feedback, author responds, repeat",
        max_turns=5,
        budget={"max_cost_usd": 1.00},
    )
    print(f"\n  Reviewer (conversational):")
    print(f"    URI: {reviewer.agent_uri}")
    print(f"    Mode: {reviewer.mode}")
    print(f"    Max turns: {reviewer.max_turns}")
    ir = reviewer.to_ir_kind()
    print(f"    IR mode: {ir['mode']}")


def demo_workflow_with_agent_tools():
    """Build a research pipeline using agent tools in a workflow graph."""
    print("\n" + "=" * 60)
    print("Agent-as-Tool — Research Pipeline Workflow")
    print("=" * 60)

    graph = WorkflowGraph("research-pipeline")

    # Step 1: Classify the paper (sync — fast)
    graph.add_agent_tool("classify",
        agent="jamjet://research/classifier",
        mode="sync",
        output_key="classification",
        timeout_ms=5000,
    )

    # Step 2: Deep analysis (streaming — long with progress)
    graph.add_agent_tool("analyze",
        agent="jamjet://research/deep-analyst",
        mode="streaming",
        output_key="analysis",
        budget={"max_cost_usd": 2.00},
    )

    # Step 3: Peer review (conversational — multi-turn)
    graph.add_agent_tool("review",
        agent="jamjet://research/peer-reviewer",
        mode="conversational",
        output_key="review_result",
    )

    graph.add_edge("classify", "analyze")
    graph.add_edge("analyze", "review")

    ir = graph.compile()
    print(f"\n  Compiled: {len(ir['nodes'])} nodes")
    for nid, n in ir["nodes"].items():
        kind = n["kind"]
        mode = kind.get("mode", "n/a")
        if isinstance(mode, dict):
            mode = f"conversational(max_turns={mode['conversational']['max_turns']})"
        print(f"    {nid}: {kind['type']} (mode={mode})")
    print(f"  Edges: {' -> '.join(e['from'] for e in ir['edges'])} -> {ir['edges'][-1]['to']}")


def demo_auto_routing():
    """Show compile-time auto expansion: agent='auto' becomes coordinator + agent_tool."""
    print("\n" + "=" * 60)
    print("Agent-as-Tool — Auto-Routing with Coordinator")
    print("=" * 60)

    graph = WorkflowGraph("auto-pipeline")

    # "auto" means: let the coordinator discover and select the best agent at runtime
    graph.add_agent_tool("process",
        agent="auto",
        mode="sync",
        output_key="result",
    )

    ir_before_info = "1 node (agent_tool with auto)"
    ir = graph.compile()

    print(f"\n  Before compile: {ir_before_info}")
    print(f"  After compile: {len(ir['nodes'])} nodes")
    for nid, n in ir["nodes"].items():
        kind = n["kind"]
        print(f"    {nid}: {kind['type']}")
        if kind["type"] == "coordinator":
            print(f"      strategy: {kind.get('strategy', 'default')}")
            print(f"      output_key: {kind.get('output_key')}")
    print(f"  Start node: {ir['start_node']}")
    print(f"  Edges: {[(e['from'], e['to']) for e in ir['edges']]}")
    print("\n  The compiler automatically inserted a Coordinator node")
    print("  that will discover and select the best agent at runtime.")


def main():
    demo_agent_tool_definitions()
    demo_workflow_with_agent_tools()
    demo_auto_routing()


if __name__ == "__main__":
    main()
