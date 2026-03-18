"""
Agent-as-Tool -- Three invocation modes for research paper processing.

Demonstrates:
- agent_tool() wrapper for all three modes (sync, streaming, conversational)
- Workflow graph with mixed agent-tool modes
- Auto-routing: compile-time expansion of agent="auto"
"""
from __future__ import annotations

from jamjet.agent_tool import agent_tool
from jamjet.workflow.graph import WorkflowGraph
from jamjet.workflow.nodes import ModelNode


def demo_agent_tool_definitions():
    """Define agents as tools with different modes."""
    print("=" * 60)
    print("Agent-as-Tool -- Definition Examples")
    print("=" * 60)

    # Sync: quick, stateless
    classifier = agent_tool(
        agent="jamjet://research/classifier",
        mode="sync",
        description="Classifies papers by field and methodology",
        timeout_ms=5000,
    )
    print(f"\n  1. Classifier (sync):")
    print(f"     URI: {classifier.agent_uri}, timeout: {classifier.timeout_ms}ms")

    # Streaming: long-running with early termination
    researcher = agent_tool(
        agent="jamjet://research/deep-analyst",
        mode="streaming",
        description="Deep literature analysis with streamed progress",
        budget={"max_cost_usd": 2.00},
        timeout_ms=60000,
    )
    print(f"\n  2. Researcher (streaming):")
    print(f"     URI: {researcher.agent_uri}, budget: ${researcher.budget['max_cost_usd']}")
    print(f"     Early termination when budget exceeded")

    # Conversational: multi-turn
    reviewer = agent_tool(
        agent="jamjet://research/peer-reviewer",
        mode="conversational",
        description="Iterative peer review with multi-turn feedback",
        max_turns=5,
    )
    print(f"\n  3. Reviewer (conversational):")
    print(f"     URI: {reviewer.agent_uri}, max_turns: {reviewer.max_turns}")
    ir = reviewer.to_ir_kind()
    print(f"     IR mode: {ir['mode']}")


def demo_mixed_mode_workflow():
    """Build a pipeline mixing all three modes."""
    print("\n" + "=" * 60)
    print("Agent-as-Tool -- Mixed Mode Pipeline")
    print("=" * 60)

    graph = WorkflowGraph("research-pipeline")
    graph.add_agent_tool("classify",
        agent="jamjet://research/classifier", mode="sync",
        output_key="classification", timeout_ms=5000)
    graph.add_agent_tool("analyze",
        agent="jamjet://research/deep-analyst", mode="streaming",
        output_key="analysis", budget={"max_cost_usd": 2.00})
    graph.add_agent_tool("review",
        agent="jamjet://research/peer-reviewer", mode="conversational",
        output_key="review_result")
    graph.add_edge("classify", "analyze")
    graph.add_edge("analyze", "review")

    ir = graph.compile()
    print(f"\n  Pipeline: {len(ir['nodes'])} nodes")
    for nid, n in ir["nodes"].items():
        kind = n["kind"]
        mode = kind.get("mode", "n/a")
        if isinstance(mode, dict):
            mode = f"conversational(turns={mode['conversational']['max_turns']})"
        print(f"    {nid}: mode={mode}")


def demo_auto_routing():
    """Compile-time auto expansion: agent='auto' -> coordinator + agent_tool."""
    print("\n" + "=" * 60)
    print("Agent-as-Tool -- Auto-Routing")
    print("=" * 60)

    graph = WorkflowGraph("auto-pipeline")
    graph.add_node("prepare", ModelNode(model="claude-haiku-4-5-20251001"))
    graph.add_agent_tool("process", agent="auto", mode="sync", output_key="result")
    graph.add_edge("prepare", "process")

    ir = graph.compile()
    print(f"\n  Wrote: 2 nodes (model + agent_tool with auto)")
    print(f"  Compiled: {len(ir['nodes'])} nodes (compiler inserted coordinator)")
    for nid, n in ir["nodes"].items():
        kind = n["kind"]
        tag = " <- auto-inserted" if kind["type"] == "coordinator" else ""
        print(f"    {nid}: {kind['type']}{tag}")
    print(f"  Start: {ir['start_node']}")
    print(f"  Edges: {[(e['from'], e['to']) for e in ir['edges']]}")


def main():
    demo_agent_tool_definitions()
    demo_mixed_mode_workflow()
    demo_auto_routing()


if __name__ == "__main__":
    main()
