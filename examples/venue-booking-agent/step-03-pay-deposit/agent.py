import os
from pathlib import Path

from venue_tools import check_dates, pay_deposit, search_venues

from jamjet import Agent

HERE = Path(__file__).parent
PAYMENTS = HERE / "payments.log"
SCRIPT = HERE / "script.yaml"
MODEL = os.environ.get("VENUE_MODEL") or f"scripted/{SCRIPT}"

mode = "scripted (no API key)"
if not MODEL.startswith("scripted/"):
    mode = MODEL
print(f"model: {mode}")

agent = Agent(
    "venue-booker",
    model=MODEL,
    tools=[search_venues, check_dates, pay_deposit],
    strategy="react",
    instructions="You book event venues for a product launch team.",
)
question = "Book a venue in Lisbon for 40 people on 2026-12-11."
result = agent.run_sync(question)
print(result.output)
for call in result.tool_calls:
    args = ", ".join(f"{k}={v!r}" for k, v in call["input"].items())
    print(f"called {call['tool']}({args})")
lines = PAYMENTS.read_text().splitlines() if PAYMENTS.exists() else []
print(f"payments.log: {len(lines)} line(s)")
