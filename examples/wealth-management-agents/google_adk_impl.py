"""
Wealth Management Multi-Agent System — Google Agent Development Kit (ADK)
=========================================================================

Same wealth management scenario implemented with Google ADK for comparison.

Four specialist agents collaborate to produce a comprehensive wealth
management recommendation:
  1. Risk Profiler      — Assesses client risk capacity
  2. Market Analyst     — Researches current market conditions
  3. Tax Strategist     — Identifies tax-optimization opportunities
  4. Portfolio Architect — Synthesizes everything into a final recommendation

Architecture:
  - Google ADK uses a "root agent" with sub-agents model
  - Tools are plain Python functions (no decorator needed, but must follow ADK schema)
  - Orchestration uses ADK's SequentialAgent / ParallelAgent / LoopAgent
  - State is passed via ADK's Session state (dict-based, not typed)
  - No built-in human approval gate — must be implemented manually

Prerequisites:
    pip install google-adk
    export GOOGLE_API_KEY="..."     # for Gemini models
    # or
    export GOOGLE_GENAI_USE_VERTEXAI=TRUE
    export GOOGLE_CLOUD_PROJECT="..."
    export GOOGLE_CLOUD_LOCATION="us-central1"

Run:
    python google_adk_impl.py
"""

from __future__ import annotations

import asyncio
import json

from google.adk.agents import Agent, SequentialAgent
from google.adk.runners import InMemoryRunner
from google.adk.sessions import InMemorySessionService
from google.genai import types

# ═════════════════════════════════════════════════════════════════════════════
# Tools (ADK style — plain functions with docstrings for schema)
# ═════════════════════════════════════════════════════════════════════════════
#
# Google ADK tools are plain Python functions. The framework extracts the
# schema from type hints and docstrings. Unlike JamJet's @tool decorator,
# there is no ToolDefinition object — ADK introspects functions directly.
#
# NOTE: These mirror the JamJet tools in tools.py but are self-contained
# here since ADK doesn't use JamJet's @tool decorator.


# ── Simulated data store (same data as tools.py) ────────────────────────────

_CLIENT_PROFILES = {
    "C-1001": {
        "client_id": "C-1001",
        "name": "Sarah Chen",
        "age": 42,
        "annual_income": 320_000,
        "net_worth": 2_800_000,
        "investment_horizon_years": 23,
        "risk_tolerance": "moderate",
        "tax_bracket": "35%",
        "filing_status": "married_joint",
        "goals": [
            "retirement at 65",
            "children's college fund (2 kids, ages 8 and 11)",
            "vacation home down payment in 5 years",
        ],
        "existing_holdings": [
            {"asset": "VTSAX", "type": "us_equity_index", "value": 680_000, "account": "taxable"},
            {"asset": "VBTLX", "type": "us_bond_index", "value": 220_000, "account": "ira"},
            {"asset": "Company RSUs", "type": "single_stock", "value": 450_000, "account": "taxable"},
            {"asset": "AAPL", "type": "single_stock", "value": 180_000, "account": "taxable"},
            {"asset": "Cash", "type": "cash", "value": 350_000, "account": "savings"},
            {"asset": "529 Plan", "type": "education", "value": 120_000, "account": "529"},
            {"asset": "Real Estate", "type": "property", "value": 800_000, "account": "n/a"},
        ],
    },
}

_MARKET_DATA = {
    "us_large_cap": {"sector": "US Large Cap", "ytd_return_pct": 12.3, "volatility": 15.2, "pe_ratio": 21.5, "outlook": "bullish"},
    "us_small_cap": {"sector": "US Small Cap", "ytd_return_pct": 8.1, "volatility": 22.8, "pe_ratio": 16.3, "outlook": "neutral"},
    "international": {"sector": "International Developed", "ytd_return_pct": 6.7, "volatility": 14.1, "pe_ratio": 14.8, "outlook": "neutral"},
    "emerging_markets": {"sector": "Emerging Markets", "ytd_return_pct": 4.2, "volatility": 20.5, "pe_ratio": 12.1, "outlook": "bullish"},
    "us_bonds": {"sector": "US Aggregate Bonds", "ytd_return_pct": 3.8, "volatility": 5.2, "pe_ratio": 0, "outlook": "neutral"},
    "tips": {"sector": "Treasury Inflation-Protected", "ytd_return_pct": 4.1, "volatility": 6.8, "pe_ratio": 0, "outlook": "bullish"},
    "reits": {"sector": "Real Estate (REITs)", "ytd_return_pct": 5.9, "volatility": 18.3, "pe_ratio": 17.2, "outlook": "neutral"},
    "technology": {"sector": "Technology", "ytd_return_pct": 18.5, "volatility": 21.0, "pe_ratio": 28.3, "outlook": "bullish"},
    "healthcare": {"sector": "Healthcare", "ytd_return_pct": 7.2, "volatility": 13.5, "pe_ratio": 18.1, "outlook": "bullish"},
    "municipal_bonds": {"sector": "Municipal Bonds", "ytd_return_pct": 3.2, "volatility": 4.1, "pe_ratio": 0, "outlook": "neutral"},
}


def get_client_profile(client_id: str) -> dict:
    """Retrieve a client's full financial profile from the CRM system.

    Args:
        client_id: The client identifier (e.g. "C-1001").

    Returns:
        Client profile with demographics, income, holdings, and goals.
    """
    profile = _CLIENT_PROFILES.get(client_id)
    if not profile:
        return {"error": f"Client {client_id} not found"}
    return profile


def assess_risk_score(
    age: int,
    income: float,
    net_worth: float,
    horizon_years: int,
    risk_tolerance: str,
    concentration_pct: float,
) -> dict:
    """Compute a quantitative risk score (0-100) and risk category.

    Args:
        age: Client's age in years.
        income: Annual gross income.
        net_worth: Total net worth.
        horizon_years: Investment time horizon.
        risk_tolerance: Self-reported tolerance (conservative/moderate/aggressive).
        concentration_pct: Largest single position as % of portfolio.

    Returns:
        Risk score, category, factor breakdown, and recommendation.
    """
    age_score = max(0, min(40, (65 - age) * 1.5))
    horizon_score = min(20, horizon_years * 2)
    tolerance_map = {"conservative": 5, "moderate": 15, "aggressive": 25}
    tolerance_score = tolerance_map.get(risk_tolerance, 10)
    recovery_score = min(15, (income / max(net_worth, 1)) * 100)
    concentration_penalty = max(0, (concentration_pct - 20) * 0.5)
    raw_score = age_score + horizon_score + tolerance_score + recovery_score - concentration_penalty
    score = max(0, min(100, raw_score))
    category = "aggressive" if score >= 70 else ("moderate" if score >= 40 else "conservative")
    return {
        "risk_score": round(score, 1),
        "category": category,
        "factors": {
            "age_capacity": round(age_score, 1),
            "horizon_capacity": round(horizon_score, 1),
            "tolerance_input": tolerance_score,
            "recovery_capacity": round(recovery_score, 1),
            "concentration_penalty": round(concentration_penalty, 1),
        },
    }


def get_market_data(sectors: str) -> dict:
    """Fetch current market data for specified sectors.

    Args:
        sectors: Comma-separated sector names (e.g. "technology,healthcare,us_bonds").

    Returns:
        Market data including YTD returns, volatility, and outlook for each sector.
    """
    requested = [s.strip().lower().replace(" ", "_") for s in sectors.split(",")]
    results = [_MARKET_DATA.get(name, {"sector": name, "error": "not found"}) for name in requested]
    return {"market_data": results, "as_of": "2026-03-13T16:00:00Z"}


def analyze_tax_implications(
    income: float,
    tax_bracket: str,
    filing_status: str,
    holdings_summary: str,
    horizon_years: int,
) -> dict:
    """Analyze tax-optimization strategies for a client.

    Args:
        income: Annual gross income.
        tax_bracket: Marginal federal tax bracket (e.g. "35%").
        filing_status: IRS filing status.
        holdings_summary: Text summary of holdings with account types.
        horizon_years: Investment horizon in years.

    Returns:
        List of tax strategies with estimated savings.
    """
    bracket_pct = float(tax_bracket.replace("%", "")) / 100
    strategies = []
    if bracket_pct >= 0.24:
        strategies.append({"strategy": "Tax-Loss Harvesting", "estimated_savings": round(income * 0.015, 2), "risk_level": "low"})
    if horizon_years >= 5:
        amt = min(income * 0.15, 50_000)
        strategies.append({"strategy": "Roth Conversion Ladder", "estimated_savings": round(amt * (bracket_pct - 0.12), 2), "risk_level": "moderate"})
    if bracket_pct >= 0.32:
        strategies.append({"strategy": "Municipal Bond Allocation", "estimated_savings": round(income * 0.008, 2), "risk_level": "low"})
    strategies.append({"strategy": "Asset Location Optimization", "estimated_savings": round(income * 0.01, 2), "risk_level": "low"})
    return {"strategies": strategies, "total_estimated_annual_savings": sum(s["estimated_savings"] for s in strategies)}


def build_portfolio_allocation(
    risk_category: str,
    horizon_years: int,
    net_worth: float,
    goals: str,
) -> dict:
    """Generate a recommended portfolio allocation.

    Args:
        risk_category: conservative, moderate, or aggressive.
        horizon_years: Investment time horizon.
        net_worth: Total investable assets.
        goals: Comma-separated financial goals.

    Returns:
        Recommended allocation with percentages and dollar amounts.
    """
    allocations = {
        "conservative": {"us_large_cap": 20, "us_small_cap": 5, "international": 10, "emerging": 5, "bonds": 30, "tips": 10, "munis": 10, "reits": 5, "cash": 5},
        "moderate": {"us_large_cap": 30, "us_small_cap": 10, "international": 15, "emerging": 5, "bonds": 15, "tips": 5, "munis": 5, "reits": 10, "cash": 5},
        "aggressive": {"us_large_cap": 35, "us_small_cap": 15, "international": 15, "emerging": 10, "bonds": 5, "reits": 10, "cash": 5, "alternatives": 5},
    }
    alloc = allocations.get(risk_category, allocations["moderate"])
    dollar_alloc = {k: round(net_worth * v / 100, 2) for k, v in alloc.items() if v > 0}
    return {"allocation_pct": alloc, "allocation_dollars": dollar_alloc, "total_invested": net_worth}


def check_compliance(client_id: str, risk_category: str) -> dict:
    """Run regulatory compliance checks on a proposed portfolio.

    Args:
        client_id: Client identifier.
        risk_category: The risk category of the proposed allocation.

    Returns:
        Compliance check results with pass/fail status.
    """
    return {
        "client_id": client_id,
        "overall_status": "approved",
        "checks": [
            {"rule": "FINRA Suitability", "status": "pass"},
            {"rule": "Reg BI — Best Interest", "status": "pass"},
            {"rule": "Concentration Limit", "status": "pass"},
            {"rule": "Liquidity Requirement", "status": "pass"},
            {"rule": "KYC/AML Verification", "status": "pass"},
        ],
    }


# ═════════════════════════════════════════════════════════════════════════════
# Agent Definitions (Google ADK)
# ═════════════════════════════════════════════════════════════════════════════
#
# Google ADK uses a tree of Agent objects. Each Agent gets:
#   - name: identifier
#   - model: Gemini model name (e.g. "gemini-2.0-flash")
#   - instruction: system prompt (string, not parameterized like JamJet)
#   - tools: list of plain Python functions
#
# Key differences from JamJet:
#   - No strategy selection (no plan-and-execute / react / critic)
#   - No built-in reasoning strategy compilation
#   - State is passed via session.state (a mutable dict), not typed Pydantic models
#   - Sub-agents are invoked via agent transfer, not workflow steps
#   - No compile-to-IR — agents run directly

risk_profiler_agent = Agent(
    name="risk_profiler",
    model="gemini-2.0-flash",
    instruction="""You are a Certified Financial Planner specializing in risk assessment.

Your process:
1. Retrieve the client's profile using get_client_profile
2. Calculate concentration risk (largest single position as % of total)
3. Use assess_risk_score to get a quantitative risk score
4. Summarize the risk assessment

Store your assessment in state['risk_assessment'] when done.
The client_id is available in state['client_id'].
""",
    tools=[get_client_profile, assess_risk_score],
)

market_analyst_agent = Agent(
    name="market_analyst",
    model="gemini-2.0-flash",
    instruction="""You are a CFA charterholder and senior market strategist.

1. Fetch broad market data: us_large_cap,us_small_cap,international,emerging_markets,us_bonds,tips
2. Fetch sector-specific data: technology,healthcare,reits,municipal_bonds
3. Synthesize a market outlook with opportunities and risks

Store your analysis in state['market_analysis'] when done.
Consider the risk assessment in state['risk_assessment'] for context.
""",
    tools=[get_market_data],
)

tax_strategist_agent = Agent(
    name="tax_strategist",
    model="gemini-2.0-flash",
    instruction="""You are an Enrolled Agent specializing in investment tax strategy.

1. Get the client profile using get_client_profile (client_id from state['client_id'])
2. Run analyze_tax_implications with their tax data
3. Prioritize strategies by estimated savings

Store your tax plan in state['tax_strategy'] when done.
""",
    tools=[get_client_profile, analyze_tax_implications],
)

portfolio_architect_agent = Agent(
    name="portfolio_architect",
    model="gemini-2.0-flash",
    instruction="""You are a senior portfolio manager.

You have access to:
- state['risk_assessment'] — risk profiler's output
- state['market_analysis'] — market analyst's output
- state['tax_strategy']    — tax strategist's output

1. Use build_portfolio_allocation to generate an allocation
2. Run check_compliance to verify regulatory compliance
3. Produce a comprehensive recommendation

Store the final recommendation in state['final_recommendation'].
""",
    tools=[build_portfolio_allocation, check_compliance],
)


# ═════════════════════════════════════════════════════════════════════════════
# Orchestration (Google ADK)
# ═════════════════════════════════════════════════════════════════════════════
#
# ADK provides built-in multi-agent orchestration patterns:
#   - SequentialAgent: runs sub-agents one after another
#   - ParallelAgent: runs sub-agents concurrently (not used here because
#     each agent depends on the previous one's output in state)
#   - LoopAgent: runs sub-agents in a loop until exit condition
#
# Unlike JamJet's Workflow:
#   - No typed state — just a mutable dict
#   - No conditional routing with lambda predicates
#   - No built-in human approval gate
#   - No durable execution (state is in-memory only)
#   - No compile-to-IR for remote execution
#   - No event sourcing for audit trail

wealth_advisor = SequentialAgent(
    name="wealth_advisor",
    sub_agents=[
        risk_profiler_agent,
        market_analyst_agent,
        tax_strategist_agent,
        portfolio_architect_agent,
    ],
    description="Orchestrates four specialist agents to produce a wealth management recommendation.",
)


# ═════════════════════════════════════════════════════════════════════════════
# Runner
# ═════════════════════════════════════════════════════════════════════════════


async def main(client_id: str = "C-1001") -> None:
    """Run the Google ADK wealth management workflow."""
    print(f"\n{'='*70}")
    print(f"  Google ADK Wealth Management Advisory — Client {client_id}")
    print(f"{'='*70}\n")

    # ADK uses a session service to manage state
    session_service = InMemorySessionService()

    # Create a runner for the orchestrator agent
    runner = InMemoryRunner(
        agent=wealth_advisor,
        app_name="wealth_management",
        session_service=session_service,
    )

    # Create a session with initial state
    session = await session_service.create_session(
        app_name="wealth_management",
        user_id="advisor-1",
        state={"client_id": client_id},
    )

    # Send the initial prompt to kick off the sequential agent
    prompt = types.Content(
        role="user",
        parts=[types.Part(text=(
            f"Please produce a comprehensive wealth management recommendation "
            f"for client {client_id}. Start by assessing their risk profile, "
            f"then analyze markets, develop a tax strategy, and finally build "
            f"the portfolio recommendation."
        ))],
    )

    print("Running agents sequentially...\n")

    # Process events from the runner
    final_response = None
    async for event in runner.run_async(
        session_id=session.id,
        user_id="advisor-1",
        new_message=prompt,
    ):
        if event.content and event.content.parts:
            agent_name = event.author or "unknown"
            text = event.content.parts[0].text if event.content.parts[0].text else ""
            if text:
                print(f"\n{'─'*70}")
                print(f"  Agent: {agent_name}")
                print(f"{'─'*70}")
                print(text[:500] + ("..." if len(text) > 500 else ""))
                final_response = text

    # Print final state
    updated_session = await session_service.get_session(
        app_name="wealth_management",
        user_id="advisor-1",
        session_id=session.id,
    )
    if updated_session and updated_session.state:
        print(f"\n{'='*70}")
        print("  SESSION STATE KEYS")
        print(f"{'='*70}")
        for key in updated_session.state:
            print(f"  - {key}")

    print(f"\n{'='*70}")
    print("  WORKFLOW COMPLETE")
    print(f"{'='*70}\n")


if __name__ == "__main__":
    import sys
    cid = "C-1001"
    for arg in sys.argv[1:]:
        if arg.startswith("C-"):
            cid = arg
    asyncio.run(main(cid))
