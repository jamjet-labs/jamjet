"""An agent runs end to end on a scripted model with no litellm installed."""

import sys
from pathlib import Path

from jamjet import Agent, tool

SCRIPT = Path(__file__).parent / "fixtures" / "scripts" / "venue.yaml"


@tool
async def search_venues(city: str, guests: int) -> str:
    """Find venues in a city."""
    return '[{"name": "Casa do Rio"}]'


@tool
async def check_dates(venue: str) -> str:
    """Check free dates for a venue."""
    return '{"free": ["2026-12-04"]}'


def test_agent_runs_on_a_scripted_model_without_litellm(monkeypatch):
    monkeypatch.setitem(sys.modules, "litellm", None)
    agent = Agent(
        "venue-booker",
        model=f"scripted/{SCRIPT}",
        tools=[search_venues, check_dates],
        instructions="You book venues.",
        strategy="react",
    )
    result = agent.run_sync("Book a venue in Lisbon for 40.")
    assert result.output == "Casa do Rio is free on 4 December."
    assert [c["tool"] for c in result.tool_calls] == ["search_venues", "check_dates"]
