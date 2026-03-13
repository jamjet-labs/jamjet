"""
Wealth Management Multi-Agent System — JamJet Implementation
=============================================================

Four specialist agents collaborate through a durable workflow to produce
a comprehensive wealth management recommendation for a client.

Agents:
  1. Risk Profiler      — Assesses client risk capacity and willingness
  2. Market Analyst     — Researches current market conditions
  3. Tax Strategist     — Identifies tax-optimization opportunities
  4. Portfolio Architect — Synthesizes all inputs into a final recommendation

Architecture:
  - Each agent uses JamJet's @tool decorator for structured capabilities
  - Agents use different reasoning strategies suited to their role
  - A Workflow orchestrates the agents with typed state and conditional routing
  - Human-in-the-loop approval gate before final recommendation delivery
  - Everything compiles to IR and can run locally or on the JamJet runtime

Run (local in-process):
    python jamjet_impl.py

Run (on JamJet runtime):
    jamjet dev
    python jamjet_impl.py --runtime
"""

from __future__ import annotations

import asyncio
import json
import sys

from pydantic import BaseModel

from jamjet import Agent, Workflow, tool
from tools import (
    analyze_tax_implications,
    assess_risk_score,
    build_portfolio_allocation,
    check_compliance,
    get_client_profile,
    get_market_data,
)

# ═════════════════════════════════════════════════════════════════════════════
# Agent Definitions
# ═════════════════════════════════════════════════════════════════════════════

# ── 1. Risk Profiler Agent ───────────────────────────────────────────────────
#
# Strategy: plan-and-execute (structured multi-step reasoning)
# - Step 1: Retrieve client profile
# - Step 2: Compute risk score
# - Step 3: Synthesize risk assessment
#
# The plan-and-execute strategy is ideal here because risk profiling is a
# well-defined sequential process where each step builds on the previous one.

risk_profiler = Agent(
    name="risk_profiler",
    model="claude-sonnet-4-6",
    tools=[get_client_profile, assess_risk_score],
    instructions="""You are a Certified Financial Planner specializing in risk assessment.

Your process:
1. Retrieve the client's full profile using get_client_profile
2. Analyze their holdings to calculate concentration risk (single largest
   position as percentage of total portfolio)
3. Use assess_risk_score with the client's data to get a quantitative score
4. Provide a clear risk assessment summary including:
   - Risk score and category
   - Key factors driving the assessment
   - Any concerns about current portfolio (concentration, etc.)
   - Recommended risk-appropriate investment approach

Be precise with numbers. Flag any red flags like excessive single-stock
concentration or misalignment between stated risk tolerance and actual capacity.
""",
    strategy="plan-and-execute",
    max_iterations=5,
)

# ── 2. Market Analyst Agent ──────────────────────────────────────────────────
#
# Strategy: react (tight tool-use loop)
# - Observe market data → analyze → look at more sectors → synthesize
#
# The react strategy is ideal for market analysis because the agent needs to
# iteratively explore data, adjusting which sectors to investigate based on
# what it finds.

market_analyst = Agent(
    name="market_analyst",
    model="claude-sonnet-4-6",
    tools=[get_market_data],
    instructions="""You are a CFA charterholder and senior market strategist.

Your approach:
1. Start by fetching broad market data for major asset classes:
   us_large_cap, us_small_cap, international, emerging_markets, us_bonds, tips
2. Then investigate specific sectors that are relevant to the client's situation:
   technology, healthcare, reits, municipal_bonds
3. Synthesize your findings into a market outlook covering:
   - Current market environment summary
   - Best-performing sectors and why
   - Risks and headwinds to watch
   - Sector-specific opportunities aligned with the client's profile
   - Recommended overweight/underweight positions

Support every recommendation with data (returns, volatility, P/E ratios).
Be direct about both opportunities and risks.
""",
    strategy="react",
    max_iterations=5,
)

# ── 3. Tax Strategist Agent ─────────────────────────────────────────────────
#
# Strategy: plan-and-execute (systematic analysis)
# - Step 1: Get client profile for tax situation
# - Step 2: Analyze tax implications across all strategies
# - Step 3: Prioritize by estimated savings
#
# Tax strategy is a systematic domain with clear rules — plan-and-execute
# ensures thorough coverage of all applicable strategies.

tax_strategist = Agent(
    name="tax_strategist",
    model="claude-sonnet-4-6",
    tools=[get_client_profile, analyze_tax_implications],
    instructions="""You are an Enrolled Agent (EA) specializing in investment tax strategy.

Your process:
1. Retrieve the client's profile to understand their tax situation
2. Run analyze_tax_implications with their specific data
3. For each strategy, explain:
   - What it involves in plain language
   - Estimated annual tax savings
   - Any risks or requirements
   - Implementation timeline
4. Provide a prioritized action plan ordered by impact

Focus on actionable strategies. Distinguish between immediate actions and
long-term structural changes. Always note when a strategy requires
coordination with their CPA or estate attorney.
""",
    strategy="plan-and-execute",
    max_iterations=5,
)

# ── 4. Portfolio Architect Agent ─────────────────────────────────────────────
#
# Strategy: critic (draft → evaluate → refine)
#
# The critic strategy is ideal for the final recommendation because the
# architect needs to:
# - Draft an initial allocation
# - Critically evaluate it against all inputs
# - Refine based on identified gaps
# This produces higher-quality output for the most important deliverable.

portfolio_architect = Agent(
    name="portfolio_architect",
    model="claude-sonnet-4-6",
    tools=[build_portfolio_allocation, check_compliance],
    instructions="""You are a senior portfolio manager with 20+ years of experience.

You will receive a comprehensive brief containing:
- Risk assessment (score, category, concerns)
- Market analysis (conditions, opportunities, risks)
- Tax strategy (optimization opportunities, estimated savings)

Your process:
1. Draft an initial portfolio allocation using build_portfolio_allocation,
   incorporating the risk category and horizon from the risk assessment
2. Run compliance checks using check_compliance
3. Produce a final recommendation that includes:

   EXECUTIVE SUMMARY
   - One paragraph overview of the recommendation

   RECOMMENDED ALLOCATION
   - Asset class percentages and dollar amounts
   - Expected return and volatility
   - Rebalancing schedule

   IMPLEMENTATION PLAN
   - Specific trades to execute (what to sell, what to buy)
   - Tax-efficient execution order (harvest losses first, then rebalance)
   - Timeline (immediate vs. phased over 3-6 months)

   KEY CONSIDERATIONS
   - How this addresses each stated goal
   - Risks and what triggers a review
   - Tax strategy integration points

Be specific. Use dollar amounts, not just percentages. The client should
be able to hand this to their broker and execute.
""",
    strategy="critic",
    max_iterations=5,
)


# ═════════════════════════════════════════════════════════════════════════════
# Workflow Orchestration
# ═════════════════════════════════════════════════════════════════════════════

workflow = Workflow("wealth_management_advisory", version="0.1.0")


@workflow.state
class AdvisoryState(BaseModel):
    """State that flows through the wealth management workflow."""

    client_id: str
    risk_assessment: str | None = None
    market_analysis: str | None = None
    tax_strategy: str | None = None
    final_recommendation: str | None = None
    compliance_status: str | None = None


@workflow.step
async def assess_risk(state: AdvisoryState) -> AdvisoryState:
    """Step 1: Risk Profiler analyzes the client's risk profile."""
    result = await risk_profiler.run(
        f"Assess the risk profile for client {state.client_id}. "
        "Retrieve their profile first, then compute a quantitative risk score."
    )
    return state.model_copy(update={"risk_assessment": result.output})


@workflow.step
async def analyze_markets(state: AdvisoryState) -> AdvisoryState:
    """Step 2: Market Analyst researches current market conditions.

    Uses findings from risk assessment to focus on relevant sectors.
    """
    result = await market_analyst.run(
        f"Analyze current market conditions for a client with this risk profile:\n\n"
        f"{state.risk_assessment}\n\n"
        "Focus on sectors and asset classes appropriate for their risk level and goals."
    )
    return state.model_copy(update={"market_analysis": result.output})


@workflow.step
async def plan_tax_strategy(state: AdvisoryState) -> AdvisoryState:
    """Step 3: Tax Strategist identifies optimization opportunities."""
    result = await tax_strategist.run(
        f"Develop a tax optimization strategy for client {state.client_id}.\n\n"
        f"Risk assessment context:\n{state.risk_assessment}"
    )
    return state.model_copy(update={"tax_strategy": result.output})


@workflow.step(
    name="build_recommendation",
    human_approval=True,  # Require advisor sign-off before delivering to client
)
async def build_recommendation(state: AdvisoryState) -> AdvisoryState:
    """Step 4: Portfolio Architect synthesizes all inputs into final recommendation.

    This step requires human approval (the senior advisor must review and
    approve before the recommendation is delivered to the client).
    """
    brief = (
        f"CLIENT ID: {state.client_id}\n\n"
        f"═══ RISK ASSESSMENT ═══\n{state.risk_assessment}\n\n"
        f"═══ MARKET ANALYSIS ═══\n{state.market_analysis}\n\n"
        f"═══ TAX STRATEGY ═══\n{state.tax_strategy}"
    )
    result = await portfolio_architect.run(
        f"Based on the following comprehensive analysis, build a final "
        f"portfolio recommendation:\n\n{brief}"
    )
    return state.model_copy(
        update={
            "final_recommendation": result.output,
            "compliance_status": "approved",
        }
    )


# ═════════════════════════════════════════════════════════════════════════════
# Runner
# ═════════════════════════════════════════════════════════════════════════════


async def run_local(client_id: str = "C-1001") -> None:
    """Execute the full advisory workflow locally (in-process)."""
    print(f"\n{'='*70}")
    print(f"  JamJet Wealth Management Advisory — Client {client_id}")
    print(f"{'='*70}\n")

    initial_state = AdvisoryState(client_id=client_id)
    result = await workflow.run(initial_state, max_steps=20)

    print(f"\n{'─'*70}")
    print(f"  WORKFLOW COMPLETE")
    print(f"  Steps executed: {result.steps_executed}")
    print(f"  Duration: {result.total_duration_us / 1_000_000:.2f}s")
    print(f"{'─'*70}\n")

    # Print each agent's output
    sections = [
        ("RISK ASSESSMENT", result.state.risk_assessment),
        ("MARKET ANALYSIS", result.state.market_analysis),
        ("TAX STRATEGY", result.state.tax_strategy),
        ("FINAL RECOMMENDATION", result.state.final_recommendation),
    ]
    for title, content in sections:
        print(f"\n{'═'*70}")
        print(f"  {title}")
        print(f"{'═'*70}")
        print(content or "(not available)")
        print()

    print(f"Compliance status: {result.state.compliance_status}")


async def run_on_runtime(client_id: str = "C-1001") -> None:
    """Submit the workflow to the JamJet runtime for durable execution."""
    from jamjet import JamjetClient

    async with JamjetClient("http://localhost:7700") as client:
        # Compile and register the workflow
        ir = workflow.compile()
        await client.create_workflow(ir)

        # Start execution
        result = await client.start_execution(
            workflow_id="wealth_management_advisory",
            input={"client_id": client_id},
        )
        exec_id = result["execution_id"]
        print(f"Execution started: {exec_id}")
        print("The workflow will pause at the approval gate.")
        print(f"To approve: jamjet approve {exec_id} --decision approved")


if __name__ == "__main__":
    client_id = "C-1001"
    runtime_mode = "--runtime" in sys.argv

    for arg in sys.argv[1:]:
        if arg.startswith("C-"):
            client_id = arg

    if runtime_mode:
        asyncio.run(run_on_runtime(client_id))
    else:
        asyncio.run(run_local(client_id))
