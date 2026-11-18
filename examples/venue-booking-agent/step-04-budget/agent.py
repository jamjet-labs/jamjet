import os
from pathlib import Path

from jamjet import Agent
from jamjet.model import BudgetExceededError

from venue_tools import (
    PAYMENTS,
    check_dates,
    pay_deposit,
    search_venues,
)

HERE = Path(__file__).parent
SCRIPT = HERE / "script.yaml"
MODEL = os.environ.get("VENUE_MODEL") or f"scripted/{SCRIPT}"

mode = "scripted (no API key)"
if not MODEL.startswith("scripted/"):
    mode = MODEL
print(f"model: {mode}")
PAYMENTS.unlink(missing_ok=True)  # start each run with a fresh log

agent = Agent(
    "venue-booker",
    model=MODEL,
    tools=[search_venues, check_dates, pay_deposit],
    strategy="react",
    instructions="You book event venues for a product launch team.",
    budget={"tokens": 2000},
)
question = "Keep checking dates until you find something."
try:
    result = agent.run_sync(question)
    print(result.output)
except BudgetExceededError as err:
    print(err)
