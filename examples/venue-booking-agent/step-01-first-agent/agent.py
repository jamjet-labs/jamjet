import os
from pathlib import Path

from jamjet import Agent

HERE = Path(__file__).parent
SCRIPT = HERE / "script.yaml"
MODEL = os.environ.get("VENUE_MODEL") or f"scripted/{SCRIPT}"

mode = "scripted (no API key)"
if not MODEL.startswith("scripted/"):
    mode = MODEL
print(f"model: {mode}")

agent = Agent(
    "venue-booker",
    model=MODEL,
    tools=[],
    instructions=(
        "You book event venues for a product launch team. "
        "Reply in plain text, in 40 words or fewer."
    ),
)
question = "We need a venue in Lisbon for 40 people in December."
result = agent.run_sync(question + " What should I ask you?")
print(result.output)
