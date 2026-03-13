"""
Shared tools for the Wealth Management multi-agent use case.

These tools simulate the data sources and computation that real wealth
management agents would need: client profiles, market data, tax rules,
portfolio analytics, and compliance checks.

In production these would hit Bloomberg, Plaid, tax APIs, etc.
"""

from __future__ import annotations

import asyncio
from typing import Any

from pydantic import BaseModel

from jamjet.tools.decorators import tool

# ── Data models ──────────────────────────────────────────────────────────────


class ClientProfile(BaseModel):
    client_id: str
    name: str
    age: int
    annual_income: float
    net_worth: float
    investment_horizon_years: int
    risk_tolerance: str  # conservative, moderate, aggressive
    tax_bracket: str  # 10%, 12%, 22%, 24%, 32%, 35%, 37%
    filing_status: str  # single, married_joint, married_separate, head_of_household
    goals: list[str]
    existing_holdings: list[dict[str, Any]]


class MarketSnapshot(BaseModel):
    sector: str
    ytd_return_pct: float
    volatility: float
    pe_ratio: float
    outlook: str  # bullish, neutral, bearish


class TaxStrategy(BaseModel):
    strategy: str
    estimated_savings: float
    description: str
    risk_level: str


# ── Tools ────────────────────────────────────────────────────────────────────


@tool
async def get_client_profile(client_id: str) -> dict[str, Any]:
    """Retrieve a client's full financial profile from the CRM system.

    Returns demographics, income, net worth, risk tolerance, goals, and
    existing portfolio holdings.
    """
    await asyncio.sleep(0.05)

    profiles: dict[str, dict[str, Any]] = {
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
        "C-1002": {
            "client_id": "C-1002",
            "name": "James Rodriguez",
            "age": 58,
            "annual_income": 185_000,
            "net_worth": 4_200_000,
            "investment_horizon_years": 7,
            "risk_tolerance": "conservative",
            "tax_bracket": "32%",
            "filing_status": "married_joint",
            "goals": [
                "retire in 7 years",
                "maintain current lifestyle in retirement",
                "leave inheritance for grandchildren",
            ],
            "existing_holdings": [
                {"asset": "VBTLX", "type": "us_bond_index", "value": 1_200_000, "account": "401k"},
                {"asset": "VTSAX", "type": "us_equity_index", "value": 800_000, "account": "ira"},
                {"asset": "Municipal Bonds", "type": "muni_bonds", "value": 400_000, "account": "taxable"},
                {"asset": "REITs", "type": "reits", "value": 200_000, "account": "taxable"},
                {"asset": "Cash", "type": "cash", "value": 600_000, "account": "savings"},
                {"asset": "Rental Property", "type": "property", "value": 1_000_000, "account": "n/a"},
            ],
        },
    }

    profile = profiles.get(client_id)
    if not profile:
        return {"error": f"Client {client_id} not found"}
    return profile


@tool
async def assess_risk_score(
    age: int,
    income: float,
    net_worth: float,
    horizon_years: int,
    risk_tolerance: str,
    concentration_pct: float,
) -> dict[str, Any]:
    """Compute a quantitative risk score (0-100) and risk category.

    Factors in age, income stability, horizon, self-reported tolerance,
    and portfolio concentration risk.
    """
    await asyncio.sleep(0.02)

    # Age factor: younger = higher capacity
    age_score = max(0, min(40, (65 - age) * 1.5))

    # Horizon factor
    horizon_score = min(20, horizon_years * 2)

    # Tolerance factor
    tolerance_map = {"conservative": 5, "moderate": 15, "aggressive": 25}
    tolerance_score = tolerance_map.get(risk_tolerance, 10)

    # Income-to-net-worth ratio (higher = more capacity to recover)
    recovery_score = min(15, (income / max(net_worth, 1)) * 100)

    # Concentration penalty
    concentration_penalty = max(0, (concentration_pct - 20) * 0.5)

    raw_score = age_score + horizon_score + tolerance_score + recovery_score - concentration_penalty
    score = max(0, min(100, raw_score))

    if score >= 70:
        category = "aggressive"
    elif score >= 40:
        category = "moderate"
    else:
        category = "conservative"

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
        "recommendation": (
            f"Risk score {score:.0f}/100 ({category}). "
            f"{'Increase equity exposure.' if score >= 50 else 'Prioritize capital preservation.'}"
        ),
    }


@tool
async def get_market_data(sectors: str) -> dict[str, Any]:
    """Fetch current market data for specified sectors.

    Args:
        sectors: Comma-separated sector names (e.g. "technology,healthcare,bonds").
    """
    await asyncio.sleep(0.05)

    all_sectors: dict[str, dict[str, Any]] = {
        "us_large_cap": {
            "sector": "US Large Cap",
            "ytd_return_pct": 12.3,
            "volatility": 15.2,
            "pe_ratio": 21.5,
            "outlook": "bullish",
        },
        "us_small_cap": {
            "sector": "US Small Cap",
            "ytd_return_pct": 8.1,
            "volatility": 22.8,
            "pe_ratio": 16.3,
            "outlook": "neutral",
        },
        "international": {
            "sector": "International Developed",
            "ytd_return_pct": 6.7,
            "volatility": 14.1,
            "pe_ratio": 14.8,
            "outlook": "neutral",
        },
        "emerging_markets": {
            "sector": "Emerging Markets",
            "ytd_return_pct": 4.2,
            "volatility": 20.5,
            "pe_ratio": 12.1,
            "outlook": "bullish",
        },
        "us_bonds": {
            "sector": "US Aggregate Bonds",
            "ytd_return_pct": 3.8,
            "volatility": 5.2,
            "pe_ratio": 0,
            "outlook": "neutral",
        },
        "tips": {
            "sector": "Treasury Inflation-Protected",
            "ytd_return_pct": 4.1,
            "volatility": 6.8,
            "pe_ratio": 0,
            "outlook": "bullish",
        },
        "reits": {
            "sector": "Real Estate (REITs)",
            "ytd_return_pct": 5.9,
            "volatility": 18.3,
            "pe_ratio": 17.2,
            "outlook": "neutral",
        },
        "technology": {
            "sector": "Technology",
            "ytd_return_pct": 18.5,
            "volatility": 21.0,
            "pe_ratio": 28.3,
            "outlook": "bullish",
        },
        "healthcare": {
            "sector": "Healthcare",
            "ytd_return_pct": 7.2,
            "volatility": 13.5,
            "pe_ratio": 18.1,
            "outlook": "bullish",
        },
        "municipal_bonds": {
            "sector": "Municipal Bonds",
            "ytd_return_pct": 3.2,
            "volatility": 4.1,
            "pe_ratio": 0,
            "outlook": "neutral",
        },
    }

    requested = [s.strip().lower().replace(" ", "_") for s in sectors.split(",")]
    results = []
    for name in requested:
        if name in all_sectors:
            results.append(all_sectors[name])
        else:
            results.append({"sector": name, "error": "sector not found"})

    return {"market_data": results, "as_of": "2026-03-13T16:00:00Z"}


@tool
async def analyze_tax_implications(
    income: float,
    tax_bracket: str,
    filing_status: str,
    holdings: str,
    horizon_years: int,
) -> dict[str, Any]:
    """Analyze tax-optimization strategies for a client's situation.

    Args:
        income:        Annual gross income.
        tax_bracket:   Marginal federal tax bracket (e.g. "35%").
        filing_status: IRS filing status.
        holdings:      JSON string of existing holdings with account types.
        horizon_years: Investment horizon in years.
    """
    await asyncio.sleep(0.05)

    bracket_pct = float(tax_bracket.replace("%", "")) / 100
    strategies: list[dict[str, Any]] = []

    # Tax-loss harvesting
    if bracket_pct >= 0.24:
        strategies.append({
            "strategy": "Tax-Loss Harvesting",
            "estimated_savings": round(income * 0.015, 2),
            "description": (
                "Sell underperforming positions to realize losses, offsetting capital "
                "gains. Replace with correlated but non-identical funds to maintain exposure."
            ),
            "risk_level": "low",
        })

    # Roth conversion ladder
    if horizon_years >= 5:
        conversion_amount = min(income * 0.15, 50_000)
        strategies.append({
            "strategy": "Roth Conversion Ladder",
            "estimated_savings": round(conversion_amount * (bracket_pct - 0.12), 2),
            "description": (
                f"Convert ${conversion_amount:,.0f}/year from Traditional IRA to Roth IRA "
                "over the next 5 years. Pay taxes now at current rate to withdraw tax-free "
                "in retirement at potentially lower rates."
            ),
            "risk_level": "moderate",
        })

    # Municipal bond allocation
    if bracket_pct >= 0.32:
        strategies.append({
            "strategy": "Municipal Bond Allocation",
            "estimated_savings": round(income * 0.008, 2),
            "description": (
                "Shift taxable bond holdings to municipal bonds. Interest is exempt "
                "from federal (and often state) income tax, beneficial at your bracket."
            ),
            "risk_level": "low",
        })

    # Asset location optimization
    strategies.append({
        "strategy": "Asset Location Optimization",
        "estimated_savings": round(income * 0.01, 2),
        "description": (
            "Place tax-inefficient assets (bonds, REITs) in tax-advantaged accounts "
            "(IRA, 401k) and tax-efficient assets (index funds, growth stocks) in "
            "taxable accounts to minimize annual tax drag."
        ),
        "risk_level": "low",
    })

    # 529 maximization
    if "college" in holdings.lower() or "529" in holdings.lower() or "education" in holdings.lower():
        strategies.append({
            "strategy": "529 Plan Maximization",
            "estimated_savings": round(min(income * 0.005, 16_000), 2),
            "description": (
                "Maximize 529 contributions for state tax deduction. Consider "
                "superfunding (5-year gift tax averaging) if grandparents can contribute."
            ),
            "risk_level": "low",
        })

    total_savings = sum(s["estimated_savings"] for s in strategies)
    return {
        "strategies": strategies,
        "total_estimated_annual_savings": round(total_savings, 2),
        "effective_tax_rate_reduction": f"{(total_savings / income) * 100:.1f}%",
    }


@tool
async def build_portfolio_allocation(
    risk_category: str,
    horizon_years: int,
    net_worth: float,
    goals: str,
    constraints: str,
) -> dict[str, Any]:
    """Generate a recommended portfolio allocation based on risk profile and goals.

    Args:
        risk_category: conservative, moderate, or aggressive.
        horizon_years: Investment time horizon.
        net_worth:     Total investable assets.
        goals:         Comma-separated list of financial goals.
        constraints:   Any constraints (e.g. "no tobacco stocks, ESG preference").
    """
    await asyncio.sleep(0.05)

    allocations = {
        "conservative": {
            "us_large_cap": 20,
            "us_small_cap": 5,
            "international_developed": 10,
            "emerging_markets": 5,
            "us_aggregate_bonds": 30,
            "tips": 10,
            "municipal_bonds": 10,
            "reits": 5,
            "cash": 5,
        },
        "moderate": {
            "us_large_cap": 30,
            "us_small_cap": 10,
            "international_developed": 15,
            "emerging_markets": 5,
            "us_aggregate_bonds": 15,
            "tips": 5,
            "municipal_bonds": 5,
            "reits": 10,
            "cash": 5,
        },
        "aggressive": {
            "us_large_cap": 35,
            "us_small_cap": 15,
            "international_developed": 15,
            "emerging_markets": 10,
            "us_aggregate_bonds": 5,
            "tips": 0,
            "municipal_bonds": 0,
            "reits": 10,
            "cash": 5,
            "alternatives": 5,
        },
    }

    alloc = allocations.get(risk_category, allocations["moderate"])

    # Adjust for short horizon
    if horizon_years < 5:
        bond_boost = 10
        alloc["us_aggregate_bonds"] = alloc.get("us_aggregate_bonds", 0) + bond_boost
        alloc["us_large_cap"] = max(0, alloc.get("us_large_cap", 0) - 5)
        alloc["us_small_cap"] = max(0, alloc.get("us_small_cap", 0) - 5)

    # Dollar amounts
    dollar_alloc = {k: round(net_worth * v / 100, 2) for k, v in alloc.items() if v > 0}

    # Expected return / risk
    expected_returns = {
        "conservative": (5.2, 7.5),
        "moderate": (7.1, 12.8),
        "aggressive": (9.3, 18.2),
    }
    exp_ret, exp_vol = expected_returns.get(risk_category, (7.0, 12.0))

    return {
        "allocation_pct": {k: v for k, v in alloc.items() if v > 0},
        "allocation_dollars": dollar_alloc,
        "total_invested": net_worth,
        "expected_annual_return_pct": exp_ret,
        "expected_volatility_pct": exp_vol,
        "rebalancing_frequency": "quarterly",
        "notes": (
            f"Designed for {risk_category} risk profile with {horizon_years}-year horizon. "
            f"Goals: {goals}. Constraints: {constraints or 'none'}."
        ),
    }


@tool
async def check_compliance(
    client_id: str,
    proposed_allocation: str,
    risk_category: str,
) -> dict[str, Any]:
    """Run regulatory compliance checks on a proposed portfolio.

    Checks suitability rules, concentration limits, and fiduciary standards.
    """
    await asyncio.sleep(0.02)

    checks = [
        {
            "rule": "FINRA Suitability (Rule 2111)",
            "status": "pass",
            "detail": f"Allocation consistent with {risk_category} risk profile.",
        },
        {
            "rule": "Reg BI — Best Interest",
            "status": "pass",
            "detail": "No conflicts of interest detected. Recommended funds are low-cost index funds.",
        },
        {
            "rule": "Concentration Limit (<25% single position)",
            "status": "pass",
            "detail": "No single position exceeds 25% of portfolio.",
        },
        {
            "rule": "Liquidity Requirement",
            "status": "pass",
            "detail": "Cash + liquid assets exceed 6-month emergency fund threshold.",
        },
        {
            "rule": "KYC/AML Verification",
            "status": "pass",
            "detail": f"Client {client_id} identity and source of funds verified.",
        },
    ]

    return {
        "client_id": client_id,
        "overall_status": "approved",
        "checks": checks,
        "reviewed_at": "2026-03-13T16:30:00Z",
    }
