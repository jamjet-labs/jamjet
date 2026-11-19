import asyncio
import os
import sys
from pathlib import Path

from jamjet import Agent

from venue_tools import (
    CANCELLATIONS,
    cancel_booking,
    check_dates,
    pay_deposit,
    search_venues,
)

HERE = Path(__file__).parent
SCRIPT = HERE / "script.yaml"
MODEL = os.environ.get("VENUE_MODEL") or f"scripted/{SCRIPT}"
# tests point this at their own engine
RUNTIME = os.environ.get("JAMJET_RUNTIME_URL", "http://127.0.0.1:7700")

mode = "scripted (no API key)"
if not MODEL.startswith("scripted/"):
    mode = MODEL
print(f"model: {mode}")
CANCELLATIONS.unlink(missing_ok=True)  # fresh log each run

agent = Agent(
    "venue-booker",
    model=MODEL,
    tools=[search_venues, check_dates, pay_deposit, cancel_booking],
    strategy="react",
    instructions=(
        "You book event venues for a product launch team. "
        "Reply in plain text, in 40 words or fewer."
    ),
    policy={
        "require_approval_for": ["pay_deposit"],
        "blocked_tools": ["cancel_*"],
    },
)
PROMPT = "Cancel our booking at Casa do Rio."
if len(sys.argv) > 1:
    PROMPT = sys.argv[1]


def count(path: Path) -> int:
    return len(path.read_text().splitlines()) if path.exists() else 0


async def main() -> None:
    try:
        result = await agent.run_durable(PROMPT, runtime_url=RUNTIME)
        print(result.output)
        print(f"execution: {result.execution_id}")
    except RuntimeError as err:
        print(f"run failed: {err}")
        print(f"execution: {err.execution_id}")
    print(f"cancellations.log: {count(CANCELLATIONS)} line(s)")


if __name__ == "__main__":
    asyncio.run(main())
