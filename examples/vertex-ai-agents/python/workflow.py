"""
JamJet + Vertex AI (Gemini) — Workflow Example
===============================================

A two-step workflow that retrieves data and generates a summary using Gemini.
Demonstrates typed state, immutable updates, and JamJet's workflow engine.

No JamJet CLI needed — runs as a standalone Python script.

Setup:
    pip install jamjet-sdk openai
    export GOOGLE_API_KEY="your-gemini-api-key"

Run:
    python workflow.py
"""

from __future__ import annotations

import asyncio
import json
import os
import sys

from pydantic import BaseModel

from jamjet import Agent, Workflow, tool


# ── Configure Gemini ────────────────────────────────────────────────────────

os.environ.setdefault(
    "OPENAI_BASE_URL",
    "https://generativelanguage.googleapis.com/v1beta/openai/",
)
os.environ.setdefault("OPENAI_API_KEY", os.environ.get("GOOGLE_API_KEY", ""))


# ── Tools ───────────────────────────────────────────────────────────────────


@tool
async def fetch_earnings(company: str) -> str:
    """Fetch recent earnings data for a company."""
    data = {
        "acme corp": "Acme Corp Q4: Revenue $2.1B (+15%), EPS $3.42 (beat by $0.18), guidance raised.",
        "globex": "Globex Q4: Revenue $890M (+8%), EPS $1.95 (missed by $0.05), flat guidance.",
    }
    return data.get(company.lower(), f"No earnings data for '{company}'.")


@tool
async def check_sentiment(topic: str) -> str:
    """Check market sentiment on a topic."""
    return f"Sentiment for '{topic}': Moderately bullish. Analyst consensus: 72% buy, 18% hold, 10% sell."


# ── Agents ──────────────────────────────────────────────────────────────────

data_collector = Agent(
    name="data_collector",
    model="gemini-2.0-flash",
    tools=[fetch_earnings, check_sentiment],
    instructions="You collect financial data. Fetch earnings and sentiment, then summarize the raw findings.",
    strategy="react",
    max_iterations=4,
)

report_writer = Agent(
    name="report_writer",
    model="gemini-2.0-flash",
    tools=[],
    instructions=(
        "You are a financial analyst. Write a clear, structured investment brief "
        "based on the research data provided. Include: summary, key metrics, outlook, risks."
    ),
    strategy="critic",  # draft → critique → refine for quality output
    max_iterations=3,
)


# ── Workflow with Typed State ───────────────────────────────────────────────

workflow = Workflow("earnings_brief", version="0.1.0")


@workflow.state
class BriefState(BaseModel):
    company: str
    raw_data: str | None = None
    report: str | None = None


@workflow.step
async def collect_data(state: BriefState) -> BriefState:
    """Step 1: Collect earnings data and sentiment."""
    result = await data_collector.run(
        f"Collect earnings data and market sentiment for {state.company}."
    )
    return state.model_copy(update={"raw_data": result.output})


@workflow.step
async def write_report(state: BriefState) -> BriefState:
    """Step 2: Write a polished investment brief from the raw data."""
    result = await report_writer.run(
        f"Write an investment brief for {state.company} based on:\n\n{state.raw_data}"
    )
    return state.model_copy(update={"report": result.output})


# ── Run ─────────────────────────────────────────────────────────────────────


async def main() -> None:
    api_key = os.environ.get("OPENAI_API_KEY") or os.environ.get("GOOGLE_API_KEY")
    if not api_key:
        print("Error: Set GOOGLE_API_KEY to run this example.")
        sys.exit(1)

    company = " ".join(sys.argv[1:]) or "Acme Corp"

    print(f"Workflow: {workflow.name}")
    print(f"Company:  {company}")
    print()

    # Option 1: Run the workflow
    initial = BriefState(company=company)
    result = await workflow.run(initial, max_steps=10)

    print(f"Steps:    {result.steps_executed}")
    print(f"Duration: {result.total_duration_us / 1_000_000:.2f}s")
    print()

    if result.state.report:
        print(result.state.report)
    else:
        print("(No report generated)")

    # Option 2: Inspect the compiled IR (for debugging or runtime submission)
    print(f"\n{'─' * 60}")
    print("Compiled IR:")
    ir = workflow.compile()
    print(json.dumps(ir, indent=2)[:500] + "...")


if __name__ == "__main__":
    asyncio.run(main())
