"""
LDP Identity-Aware Routing with Provenance-Weighted Synthesis.

Demonstrates how LDP (LLM Delegate Protocol) identity cards enhance JamJet's
Coordinator routing and enable provenance-weighted synthesis.

What this shows:
1. Agents carry LDP identity metadata (reasoning_profile, cost/latency hints,
   quality_score) in their AgentCard labels.
2. LdpCoordinatorStrategy uses this metadata to route tasks by difficulty:
   - Easy questions -> fast, cheap agents
   - Hard questions -> analytical, high-quality agents
   - Domain-specific questions -> domain experts
3. All agents answer in parallel (fan-out), each result carries LDP provenance.
4. A synthesis step weights answers by routing rank + provenance quality/confidence.

Papers:
- LDP: https://arxiv.org/abs/2603.08852
- Interactive explainer: https://sunilprakash.com/research/ldp/

To use real LLMs instead of mock responses:
    export ANTHROPIC_API_KEY=sk-...
    # Then modify the execute_agent() function below -- see "REAL LLM" comments.
"""
from __future__ import annotations

import asyncio
from typing import Any

from agents import MOCK_RESPONSES, RESEARCH_AGENTS, MockRegistry
from ldp_strategy import LdpCoordinatorStrategy, classify_difficulty, detect_domains

# ─────────────────────────────────────────────────────────
# Uncomment the imports below to use real LLMs:
#
# from jamjet import Agent
#
# REAL_AGENTS = {
#     "jamjet://research/quick-lookup": Agent(
#         "quick-lookup",
#         model="claude-haiku-4-5-20251001",
#         instructions="You are a fast research assistant. Give brief, accurate answers.",
#         max_cost_usd=0.05,
#         max_iterations=3,
#     ),
#     "jamjet://research/deep-analyst": Agent(
#         "deep-analyst",
#         model="claude-sonnet-4-6",
#         instructions=(
#             "You are a thorough research analyst. Provide deep, structured analysis "
#             "with multiple perspectives and evidence-based reasoning."
#         ),
#         max_cost_usd=2.0,
#         max_iterations=15,
#     ),
#     "jamjet://research/domain-specialist": Agent(
#         "domain-specialist",
#         model="claude-sonnet-4-6",
#         instructions=(
#             "You are a domain expert in finance, healthcare, and legal matters. "
#             "Provide domain-specific analysis with technical precision."
#         ),
#         max_cost_usd=1.0,
#         max_iterations=10,
#     ),
# }
# ─────────────────────────────────────────────────────────


async def execute_agent(
    agent_uri: str, question: str, question_idx: int,
) -> dict[str, Any]:
    """Run an agent on a question and return output + confidence.

    Uses mock responses by default. To use real LLMs, uncomment the
    REAL_AGENTS block above and replace the body of this function with:

        agent = REAL_AGENTS[agent_uri]
        result = await agent.run(question)
        return {"output": result.output, "confidence": 0.8}
    """
    mock = MOCK_RESPONSES.get((agent_uri, question_idx), {})
    return {
        "output": mock.get("output", "(no response)"),
        "confidence": mock.get("confidence", 0.5),
    }


def attach_provenance(
    agent_uri: str, agent_card: dict[str, Any], result: dict[str, Any],
) -> dict[str, Any]:
    """Attach LDP provenance metadata to an agent's result."""
    labels = agent_card.get("labels", {})
    return {
        "produced_by": labels.get("ldp.delegate_id", agent_uri),
        "model_family": labels.get("ldp.model_family", "unknown"),
        "reasoning_profile": labels.get("ldp.reasoning_profile", "unknown"),
        "quality_score": float(labels.get("ldp.quality_score", "0.5")),
        "confidence": result.get("confidence", 0.5),
        "verified": False,
    }


def synthesize(
    question: str,
    opinions: list[dict[str, Any]],
    rankings: list[Any],
) -> str:
    """Synthesize a final answer from multiple opinions, weighted by provenance.

    In production, this would be an LLM call. Here we demonstrate the weighting
    logic and produce a structured synthesis prompt.

    To use a real LLM for synthesis, replace the return with:

        from jamjet import Agent
        synthesizer = Agent("synthesizer", model="claude-sonnet-4-6", ...)
        result = await synthesizer.run(prompt)
        return result.output
    """
    # Build rank lookup: agent_uri -> rank position (0 = best)
    rank_map = {r.agent_uri: idx for idx, r in enumerate(rankings)}

    # Weight each opinion by: routing rank + quality + confidence
    weighted = []
    for op in opinions:
        uri = op["agent_uri"]
        prov = op["provenance"]
        rank = rank_map.get(uri, len(rankings))
        rank_weight = 1.0 / (1 + rank)  # 1st=1.0, 2nd=0.5, 3rd=0.33
        quality_weight = prov["quality_score"]
        confidence_weight = prov["confidence"]
        combined = rank_weight * 0.4 + quality_weight * 0.35 + confidence_weight * 0.25
        weighted.append({**op, "weight": combined})

    weighted.sort(key=lambda w: w["weight"], reverse=True)

    # Build synthesis prompt (would be sent to LLM in production)
    lines = [
        f"Question: {question}",
        "",
        "Synthesize a final answer from these sources, "
        "giving more weight to higher-scored sources:",
        "",
    ]
    for i, w in enumerate(weighted, 1):
        prov = w["provenance"]
        lines.extend([
            f"Source {i} (weight={w['weight']:.3f}):",
            f"  Agent: {prov['produced_by']}",
            f"  Reasoning Profile: {prov['reasoning_profile']}",
            f"  Quality: {prov['quality_score']:.2f}, "
            f"Confidence: {prov['confidence']:.2f}",
            f"  Answer: {w['output'][:200]}{'...' if len(w['output']) > 200 else ''}",
            "",
        ])

    prompt = "\n".join(lines)

    # Return the highest-weighted answer as the "synthesis" for the mock demo.
    # In production, you'd send `prompt` to an LLM for actual synthesis.
    best = weighted[0]
    return (
        f"[Synthesis based on {len(weighted)} sources, "
        f"primary: {best['provenance']['produced_by']} "
        f"(weight={best['weight']:.3f})]\n\n"
        f"{best['output']}"
    )


async def run_question(
    strategy: LdpCoordinatorStrategy,
    question: str,
    question_idx: int,
) -> None:
    """Route, fan-out, attach provenance, and synthesize for one question."""
    difficulty = classify_difficulty(question)
    domains = detect_domains(question)

    print(f"\n{'=' * 70}")
    print(f"Q: {question}")
    print(f"   Difficulty: {difficulty} | Domains: {domains or '(none)'}")
    print("=" * 70)

    # Step 1: Coordinator scores all agents
    rankings, spread = await strategy.score(
        task=question, candidates=RESEARCH_AGENTS, weights={}, context={},
    )
    print(f"\nRouting Scores (spread={spread:.3f}):")
    for r in rankings:
        agent = next(a for a in RESEARCH_AGENTS if a.uri == r.agent_uri)
        labels = agent.agent_card.get("labels", {})
        profile = labels.get("ldp.reasoning_profile", "?")
        cost = labels.get("ldp.cost_profile", "?")
        print(f"  {r.composite:.3f}  {r.agent_uri}  [{profile}, cost={cost}]")

    decision = await strategy.decide(question, rankings, 0.1, "", {})
    print(f"\nPrimary selection: {decision.selected_uri}")
    print(f"  Reasoning: {decision.reasoning}")

    # Step 2: Fan-out -- all agents answer in parallel
    print("\nFan-out (all agents answering in parallel)...")
    tasks = [
        execute_agent(agent.uri, question, question_idx)
        for agent in RESEARCH_AGENTS
    ]
    results = await asyncio.gather(*tasks)

    # Step 3: Attach provenance to each result
    opinions = []
    for agent, result in zip(RESEARCH_AGENTS, results):
        provenance = attach_provenance(agent.uri, agent.agent_card, result)
        opinions.append({
            "agent_uri": agent.uri,
            "output": result["output"],
            "provenance": provenance,
        })

    print("\nAgent Responses with Provenance:")
    for op in opinions:
        prov = op["provenance"]
        is_primary = op["agent_uri"] == decision.selected_uri
        marker = " << PRIMARY" if is_primary else ""
        print(f"\n  [{prov['produced_by']}]{marker}")
        print(f"  Profile: {prov['reasoning_profile']} | "
              f"Quality: {prov['quality_score']:.2f} | "
              f"Confidence: {prov['confidence']:.2f}")
        preview = op["output"][:150]
        if len(op["output"]) > 150:
            preview += "..."
        print(f"  Answer: {preview}")

    # Step 4: Provenance-weighted synthesis
    synthesis = synthesize(question, opinions, rankings)
    print(f"\nSynthesis:\n  {synthesis[:300]}")


async def main():
    print("LDP Identity-Aware Routing with Provenance-Weighted Synthesis")
    print("=" * 70)
    print("Agents:")
    for a in RESEARCH_AGENTS:
        labels = a.agent_card.get("labels", {})
        print(f"  {a.uri}")
        print(f"    reasoning={labels.get('ldp.reasoning_profile')}, "
              f"cost={labels.get('ldp.cost_profile')}, "
              f"quality={labels.get('ldp.quality_score')}")

    strategy = LdpCoordinatorStrategy(registry=MockRegistry())

    questions = [
        "What is the capital of France?",
        "Analyze the impact of quantitative easing on emerging market debt sustainability",
        "What are the tax implications of Roth IRA conversions?",
    ]

    for idx, question in enumerate(questions):
        await run_question(strategy, question, idx)

    print(f"\n{'=' * 70}")
    print("Demo complete. Key takeaways:")
    print("  1. Easy question -> routed to quick-lookup (fast, cheap)")
    print("  2. Hard question -> routed to deep-analyst (analytical, high quality)")
    print("  3. Domain question -> routed to domain-specialist (finance expertise)")
    print("  4. All answers carry LDP provenance for weighted synthesis")
    print(f"{'=' * 70}")


if __name__ == "__main__":
    asyncio.run(main())
