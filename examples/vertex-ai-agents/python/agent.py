"""
JamJet + Vertex AI (Gemini) — Python Example
=============================================

A research agent powered by Google Gemini, using JamJet's built-in
reasoning strategies and tool orchestration.

No JamJet CLI needed — runs as a standalone Python script.

Setup:
    pip install jamjet-sdk openai
    export GOOGLE_API_KEY="your-gemini-api-key"

Run:
    python agent.py
"""

from __future__ import annotations

import asyncio
import os
import sys

from jamjet import Agent, tool


# ── Configure Gemini via OpenAI-compatible endpoint ─────────────────────────
#
# Google Gemini exposes an OpenAI-compatible API. JamJet's Agent uses the
# openai library under the hood, so we just point it at Google's endpoint.

os.environ.setdefault(
    "OPENAI_BASE_URL",
    "https://generativelanguage.googleapis.com/v1beta/openai/",
)
os.environ.setdefault("OPENAI_API_KEY", os.environ.get("GOOGLE_API_KEY", ""))


# ── Tools ───────────────────────────────────────────────────────────────────


@tool
async def search_documents(query: str) -> str:
    """Search internal documents for relevant information."""
    # Stub — replace with your vector DB, Elasticsearch, or API call.
    docs = {
        "revenue": "Q4 2025 revenue: $142M (+23% YoY). SaaS ARR: $98M.",
        "customers": "Enterprise customers: 340. Net retention: 127%.",
        "product": "Launched agent orchestration platform in Q3. 89 enterprise pilots.",
        "competitors": "Main competitors: LangChain (open-source), CrewAI (funded $18M).",
        "risks": "Key risks: LLM cost volatility, enterprise sales cycle (avg 4.2 months).",
    }
    results = [v for k, v in docs.items() if query.lower() in k.lower()]
    return "\n".join(results) if results else f"No results for '{query}'."


@tool
async def get_stock_data(ticker: str) -> str:
    """Get current stock price and key metrics for a ticker."""
    # Stub — replace with a real market data API.
    data = {
        "GOOG": "GOOG: $182.45 | P/E: 24.3 | Market cap: $2.24T | YTD: +18.2%",
        "MSFT": "MSFT: $448.20 | P/E: 35.1 | Market cap: $3.33T | YTD: +12.7%",
        "AMZN": "AMZN: $213.80 | P/E: 42.6 | Market cap: $2.22T | YTD: +22.1%",
    }
    return data.get(ticker.upper(), f"No data for ticker '{ticker}'.")


@tool
async def save_note(title: str, content: str) -> str:
    """Save a research note for later reference."""
    print(f"  [Note] {title}: {content[:80]}...")
    return f"Note saved: {title}"


# ── Agent ───────────────────────────────────────────────────────────────────
#
# This agent uses the "react" strategy — a tight observe-reason-act loop
# ideal for research tasks where the next step depends on what was found.
#
# Other strategies:
#   "plan-and-execute" — generates a plan first, then executes step by step
#   "critic"           — drafts, self-critiques, and refines the output

research_agent = Agent(
    name="gemini_researcher",
    model="gemini-2.0-flash",  # or gemini-1.5-pro, gemini-1.5-flash
    tools=[search_documents, get_stock_data, save_note],
    instructions=(
        "You are an investment research analyst. "
        "Search for relevant data, check stock metrics, and save key findings as notes. "
        "Produce a concise research summary with data points."
    ),
    strategy="react",
    max_iterations=6,
    max_cost_usd=0.50,
)


# ── Run ─────────────────────────────────────────────────────────────────────


async def main() -> None:
    api_key = os.environ.get("OPENAI_API_KEY") or os.environ.get("GOOGLE_API_KEY")
    if not api_key:
        print("Error: Set GOOGLE_API_KEY to run this example.")
        print("  export GOOGLE_API_KEY='your-gemini-api-key'")
        sys.exit(1)

    prompt = " ".join(sys.argv[1:]) or "Research the AI agent platform market and summarize key players."

    print(f"Agent:    {research_agent.name}")
    print(f"Model:    {research_agent.model}")
    print(f"Strategy: {research_agent.strategy}")
    print(f"Tools:    {research_agent.tool_names}")
    print()
    print(f"Prompt:   {prompt}")
    print(f"{'─' * 60}")

    result = await research_agent.run(prompt)

    print(f"{'─' * 60}")
    print(result.output)
    print()
    print(f"Tool calls: {len(result.tool_calls)}")
    print(f"Duration:   {result.duration_us / 1_000_000:.2f}s")


if __name__ == "__main__":
    asyncio.run(main())
