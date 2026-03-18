"""
Coordinator Routing -- Dynamic agent selection for customer support.

Demonstrates:
- DefaultCoordinatorStrategy for capability-based agent discovery and scoring
- Structured scoring with dimension weights
- Automatic fallback when no candidates match
- CoordinatorNode in a workflow graph
"""
from __future__ import annotations

import asyncio

from jamjet.coordinator import DefaultCoordinatorStrategy
from jamjet.workflow.graph import WorkflowGraph
from jamjet.workflow.nodes import CoordinatorNode, ModelNode


SUPPORT_AGENTS = [
    {
        "uri": "jamjet://support/billing-agent",
        "skills": ["billing", "payments", "refunds", "subscriptions"],
        "agent_card": {"name": "Billing Agent"},
        "latency_class": "low",
        "cost_class": "low",
        "trust_domain": "internal",
    },
    {
        "uri": "jamjet://support/technical-agent",
        "skills": ["debugging", "api-errors", "integrations", "technical-support"],
        "agent_card": {"name": "Technical Agent"},
        "latency_class": "medium",
        "cost_class": "medium",
        "trust_domain": "internal",
    },
    {
        "uri": "jamjet://support/general-agent",
        "skills": ["faq", "account", "general-support", "onboarding"],
        "agent_card": {"name": "General Agent"},
        "latency_class": "low",
        "cost_class": "low",
        "trust_domain": "internal",
    },
]


class MockRegistry:
    async def list_agents(self):
        return SUPPORT_AGENTS


async def demo_coordinator_scoring():
    """Show how the coordinator scores and selects agents."""
    print("=" * 60)
    print("Coordinator Routing -- Structured Scoring Demo")
    print("=" * 60)

    strategy = DefaultCoordinatorStrategy(registry=MockRegistry())

    tickets = [
        {"task": "Customer wants a refund for duplicate charge", "skills": ["billing", "refunds"]},
        {"task": "API returning 500 errors on /v2/tasks endpoint", "skills": ["debugging", "api-errors"]},
        {"task": "How do I reset my password?", "skills": ["faq", "account"]},
        {"task": "Need help with quantum computing", "skills": ["quantum"]},
    ]

    for ticket in tickets:
        print(f"\n--- Ticket: {ticket['task'][:60]} ---")

        candidates, filtered = await strategy.discover(
            task=ticket["task"],
            required_skills=ticket["skills"],
            preferred_skills=[],
            trust_domain="internal",
            context={},
        )
        print(f"  Discovered: {len(candidates)} candidates, {len(filtered)} filtered")

        if not candidates:
            print("  No matching agents! Would escalate to human.")
            continue

        rankings, spread = await strategy.score(
            task=ticket["task"], candidates=candidates, weights={}, context={},
        )
        print(f"  Rankings (spread={spread:.3f}):")
        for r in rankings:
            print(f"    {r.agent_uri}: {r.composite:.3f}")

        decision = await strategy.decide(
            task=ticket["task"], top_candidates=rankings,
            threshold=0.1, tiebreaker_model="claude-sonnet-4-6", context={},
        )
        print(f"  -> Selected: {decision.selected_uri} (method={decision.method})")


def demo_workflow_graph():
    """Build a workflow with a Coordinator node."""
    print("\n" + "=" * 60)
    print("Coordinator Routing -- Workflow Graph")
    print("=" * 60)

    graph = WorkflowGraph("support-routing")
    graph.add_node("classify", ModelNode(model="claude-haiku-4-5-20251001"))
    graph.add_coordinator("route",
        task="Route ticket to support agent",
        required_skills=["support"],
        output_key="selected_agent",
        strategy="default",
        tiebreaker={"model": "claude-sonnet-4-6", "threshold": 0.1},
    )
    graph.add_edge("classify", "route")

    ir = graph.compile()
    print(f"\n  Compiled: {len(ir['nodes'])} nodes, {len(ir['edges'])} edges")
    for nid, node in ir["nodes"].items():
        kind = node["kind"]
        print(f"    {nid}: {kind['type']}")


async def main():
    await demo_coordinator_scoring()
    demo_workflow_graph()


if __name__ == "__main__":
    asyncio.run(main())
