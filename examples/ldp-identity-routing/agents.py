"""
LDP-aware research agents with identity card metadata.

Each agent carries LDP identity fields in its AgentCandidate labels:
- ldp.delegate_id, ldp.model_family, ldp.reasoning_profile
- ldp.cost_profile, ldp.latency_profile, ldp.quality_score
- ldp.domains (comma-separated)

These labels are read by LdpCoordinatorStrategy during scoring.
"""
from __future__ import annotations

from jamjet.coordinator import AgentCandidate


RESEARCH_AGENTS = [
    AgentCandidate(
        uri="jamjet://research/quick-lookup",
        agent_card={
            "name": "Quick Lookup",
            "labels": {
                "ldp.delegate_id": "ldp:delegate:quick-lookup",
                "ldp.model_family": "Claude",
                "ldp.reasoning_profile": "fast",
                "ldp.cost_profile": "low",
                "ldp.latency_profile": "p50:500ms",
                "ldp.quality_score": "0.55",
                "ldp.domains": "",
            },
        },
        skills=["factual-lookup", "summarization", "simple-qa"],
        latency_class="low",
        cost_class="low",
        trust_domain="research",
    ),
    AgentCandidate(
        uri="jamjet://research/deep-analyst",
        agent_card={
            "name": "Deep Analyst",
            "labels": {
                "ldp.delegate_id": "ldp:delegate:deep-analyst",
                "ldp.model_family": "Claude",
                "ldp.reasoning_profile": "analytical",
                "ldp.cost_profile": "high",
                "ldp.latency_profile": "p50:8000ms",
                "ldp.quality_score": "0.92",
                "ldp.domains": "",
            },
        },
        skills=["deep-analysis", "reasoning", "multi-step", "synthesis"],
        latency_class="slow",
        cost_class="high",
        trust_domain="research",
    ),
    AgentCandidate(
        uri="jamjet://research/domain-specialist",
        agent_card={
            "name": "Domain Specialist",
            "labels": {
                "ldp.delegate_id": "ldp:delegate:domain-specialist",
                "ldp.model_family": "Claude",
                "ldp.reasoning_profile": "domain-expert",
                "ldp.cost_profile": "medium",
                "ldp.latency_profile": "p50:3000ms",
                "ldp.quality_score": "0.78",
                "ldp.domains": "finance,healthcare,legal",
            },
        },
        skills=["domain-analysis", "finance", "healthcare", "legal", "compliance"],
        latency_class="medium",
        cost_class="medium",
        trust_domain="research",
    ),
]


class MockRegistry:
    """In-memory registry for demo purposes."""

    async def list_agents(self):
        return RESEARCH_AGENTS


# Mock responses keyed by (agent_uri, question_index) for demo reproducibility.
# In production, replace with actual Agent.run() calls.
MOCK_RESPONSES = {
    ("jamjet://research/quick-lookup", 0): {
        "output": "The capital of France is Paris. It has been the capital since "
        "the late 10th century and is the country's largest city with a "
        "population of approximately 2.1 million in the city proper.",
        "confidence": 0.95,
    },
    ("jamjet://research/deep-analyst", 0): {
        "output": "Paris is the capital of France, serving as the seat of "
        "government since Hugh Capet established it as the royal capital in 987 CE. "
        "The city's role extends beyond politics: it is the economic, cultural, "
        "and intellectual center of France, housing major institutions like the "
        "Sorbonne, the Louvre, and the National Assembly.",
        "confidence": 0.98,
    },
    ("jamjet://research/domain-specialist", 0): {
        "output": "Paris is the capital of France. From a governance perspective, "
        "it serves as the administrative center for the centralized French state, "
        "hosting all major ministries and the presidential Elysee Palace.",
        "confidence": 0.90,
    },
    ("jamjet://research/quick-lookup", 1): {
        "output": "Quantitative easing (QE) increases money supply, which can weaken "
        "the issuing currency and drive capital flows to emerging markets. This "
        "can affect EM debt through currency appreciation and carry trades.",
        "confidence": 0.45,
    },
    ("jamjet://research/deep-analyst", 1): {
        "output": "Quantitative easing in developed economies creates a multi-layered "
        "impact on emerging market debt sustainability.\n\n"
        "**Direct channels:** (1) Portfolio rebalancing pushes investors toward "
        "higher-yield EM bonds, compressing spreads and lowering borrowing costs. "
        "(2) USD weakness from Fed QE reduces the real burden of dollar-denominated "
        "EM debt. (3) Commodity price inflation benefits commodity-exporting EMs.\n\n"
        "**Indirect risks:** (1) When QE unwinds (taper tantrum, 2013), capital "
        "flight causes sudden spread widening. (2) Easy liquidity encourages EM "
        "governments to over-borrow at artificially low rates. (3) Currency "
        "mismatches amplify when USD strengthens post-QE.\n\n"
        "**Net assessment:** QE temporarily improves EM debt sustainability metrics "
        "but creates fragility. Countries with strong fiscal positions and local-"
        "currency debt benefit more; those dependent on dollar borrowing face "
        "compounding risks when normalization begins.",
        "confidence": 0.88,
    },
    ("jamjet://research/domain-specialist", 1): {
        "output": "From a financial markets perspective, QE affects EM debt through "
        "several channels: interest rate differentials drive carry trades, currency "
        "effects alter real debt burdens, and global risk appetite shifts change "
        "spread dynamics. The 2013 taper tantrum demonstrated the fragility: EM "
        "bond spreads widened 100-150bps when the Fed signaled QE reduction.",
        "confidence": 0.75,
    },
    ("jamjet://research/quick-lookup", 2): {
        "output": "Roth IRA conversions involve paying income tax on the converted "
        "amount in the year of conversion. The converted funds then grow tax-free "
        "and qualified withdrawals are tax-free in retirement.",
        "confidence": 0.60,
    },
    ("jamjet://research/deep-analyst", 2): {
        "output": "Roth IRA conversions have several tax implications to consider. "
        "The converted amount is added to ordinary income in the conversion year. "
        "Key considerations include: marginal tax bracket impact, state tax "
        "obligations, potential Medicare premium surcharges (IRMAA) two years after "
        "conversion, and the pro-rata rule if you hold both pre-tax and after-tax "
        "IRA balances. Strategic approaches include multi-year partial conversions "
        "to stay within a target bracket, and timing conversions during low-income "
        "years (early retirement, sabbaticals).",
        "confidence": 0.82,
    },
    ("jamjet://research/domain-specialist", 2): {
        "output": "As a financial planning matter, Roth IRA conversions create a "
        "taxable event: the entire converted amount is treated as ordinary income.\n\n"
        "**Critical tax considerations:**\n"
        "1. Pro-rata rule: If you have pre-tax IRA balances, you cannot selectively "
        "convert only after-tax dollars\n"
        "2. IRMAA impact: High conversion amounts can trigger Medicare surcharges "
        "with a 2-year lookback\n"
        "3. State tax: Some states (e.g., IL, PA) don't tax Roth conversions\n"
        "4. Five-year rule: Each conversion has its own 5-year clock for penalty-free "
        "withdrawal of converted principal\n\n"
        "**Optimal strategy:** Partial conversions spread across years, targeting "
        "the top of the 22% or 24% bracket, ideally during early retirement before "
        "Social Security and RMDs begin.",
        "confidence": 0.91,
    },
}
