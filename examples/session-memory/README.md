# session-memory — multi-turn agent with memory and an artifact

Demonstrates the three Track-4 guarantees in a single two-turn session:

1. **Thread continuity** — a second run of the same agent continues the conversation thread from the first run, so the model sees prior turns even after a simulated process restart.
2. **Memory recall** — with `Agent(memory=True)` (or an injected memory backend), the governed retrieve-at-start / record-at-end loop retrieves context from the prior session turn and injects it into run 2.
3. **Artifact round-trip** — `session.artifacts.put(bytes)` stores content by hash before the restart; `session.artifacts.get(hash)` retrieves it after.

## Prerequisites

Python 3.11+, `jamjet` installed.

    pip install jamjet

For the live mode (real model + real Engram):

    pip install 'jamjet[memory]'

## Run

**Demo mode** (no API key, scripted model, in-process fake memory):

    python main.py

**Live mode** (real Anthropic model + real Engram server):

    ANTHROPIC_API_KEY=sk-...
    ENGRAM_URL=http://localhost:8765  # or wherever your Engram server runs
    python main.py --live

For the durable execution path (engine + worker + sidecar), see `react-agent-durable/README.md`. The in-process `agent.run(session=...)` form used here is simpler and demonstrates the same API.

## What happens

Turn 1: the agent is told "My project is called Hermes." The turn is persisted to the `Session` and recorded to memory. A short artifact is stored by hash.

Turn 2 (after a simulated restart with a fresh `Agent` + `SessionStore`): the agent is asked "What is my project called?" The model receives both the prior thread (turn 1) and the retrieved memory block. The artifact stored before the restart is fetched by its hash.

## Key API

```python
from jamjet import Agent, JamjetClient, Session, SessionStore, tool

store = SessionStore()               # defaults to ~/.jamjet/sessions.db
session = store.create("my-session")
# Artifacts go through a runtime client; attach one before session.artifacts.
session.attach_client(JamjetClient("http://127.0.0.1:7700"))

agent = Agent(
    "assistant",
    model="claude-sonnet-4-6",
    tools=[...],
    memory=True,                     # opt-in Engram; pip install 'jamjet[memory]'
    session_store=store,
)

r1 = await agent.run("Turn 1 prompt", session=session)
ref = await session.artifacts.put(b"some bytes", "text/plain")

# Reload after restart:
session2 = SessionStore().load("my-session")
session2.attach_client(JamjetClient("http://127.0.0.1:7700"))  # binding is not persisted; reattach
r2 = await agent.run("Turn 2 prompt", session=session2)  # sees turn 1 + memory
data = await session2.artifacts.get(ref.hash)
```
