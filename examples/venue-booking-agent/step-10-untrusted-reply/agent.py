import asyncio
import os
from pathlib import Path

from jamjet import Agent

from venue_tools import (
    PAYMENTS,
    pay_deposit,
    read_reply,
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
PAYMENTS.unlink(missing_ok=True)  # start each run with a fresh log

agent = Agent(
    "venue-booker",
    model=MODEL,
    tools=[search_venues, read_reply, pay_deposit],
    strategy="react",
    instructions=(
        "You book event venues for a product launch team. "
        "Reply in plain text, in 40 words or fewer."
    ),
    policy={"require_approval_for": ["pay_deposit"]},
)
PROMPT = (
    "Book a venue in Lisbon for 40 people on 2026-12-11."
    " Read the venue's reply, then pay a 500 deposit."
)


async def main() -> None:
    try:
        result = await agent.run_durable(PROMPT, runtime_url=RUNTIME)
        print(result.output)
        print(f"execution: {result.execution_id}")
    except RuntimeError as exc:  # a rejected payment ends the run
        print(f"run ended: {exc}")
    lines = []
    if PAYMENTS.exists():
        lines = PAYMENTS.read_text().splitlines()
    print(f"payments.log: {len(lines)} line(s)")


if __name__ == "__main__":
    asyncio.run(main())
