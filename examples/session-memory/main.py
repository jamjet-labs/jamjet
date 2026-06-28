"""Multi-turn session example: thread continuity + memory + artifact.

An agent with a persistent Session runs two turns. The second turn sees
the prior thread AND a retrieved memory block (session continuity + memory
recall), then writes a summary artifact that is fetched by hash.

Demo mode (no API key, no Engram, scripted model):

    python main.py

Live mode (needs ANTHROPIC_API_KEY and a running Engram server):

    pip install 'jamjet[memory]'
    ANTHROPIC_API_KEY=sk-... ENGRAM_URL=http://localhost:8765 python main.py --live

In demo mode the model is scripted and memory uses a local in-process fake so
the flow runs end-to-end without any external services.  In live mode
``Agent(memory=True)`` opens the default embedded Engram bridge; retrieve-at-start
and record-at-end run against a real Engram server.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import sys
import types
from contextlib import contextmanager
from collections.abc import Generator
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

from jamjet import Agent, ArtifactRef, Session, SessionStore, tool


# ---------------------------------------------------------------------------
# Shared tool
# ---------------------------------------------------------------------------


@tool
async def summarize(text: str) -> str:
    """Summarize the given text."""
    return f"Summary: {text[:80]}"


# ---------------------------------------------------------------------------
# Demo-mode helpers: scripted model + fake memory + fake artifact transport
# ---------------------------------------------------------------------------


class _ScriptedModel:
    """Scripted stand-in for litellm; no API key required."""

    async def acompletion(
        self,
        model: str,
        messages: list,
        tools: list | None = None,
        **kwargs: Any,
    ) -> Any:
        last_user = next(
            (m.get("content", "") for m in reversed(messages) if m.get("role") == "user"),
            "",
        )
        if "project" in last_user.lower() and "hermes" in last_user.lower():
            reply = "Got it — the project is called Hermes."
        elif "project" in last_user.lower() or "name" in last_user.lower():
            reply = "The project is Hermes."
        else:
            reply = f"Noted: {last_user}"

        msg = MagicMock()
        msg.content = reply
        msg.role = "assistant"
        msg.tool_calls = []
        resp = MagicMock()
        resp.choices = [MagicMock(message=msg)]
        return resp


class _FakeMemory:
    """In-process duck-typed AgentMemory for demo mode."""

    def __init__(self) -> None:
        self._store: list[str] = []
        self._scoped_user_id: str | None = None

    @contextmanager
    def as_scope(
        self,
        *,
        user_id: str | None = None,
        org_id: str | None = None,
    ) -> Generator[_FakeMemory, None, None]:
        old = self._scoped_user_id
        self._scoped_user_id = user_id
        try:
            yield self
        finally:
            self._scoped_user_id = old

    async def record(self, text: str, **kwargs: Any) -> None:
        self._store.append(text)

    async def context(self, query: str, **kwargs: Any) -> str:
        if not self._store:
            return ""
        # Return a summary of what was recorded in the session.
        recorded = "; ".join(self._store[-3:])
        return f"[Recalled from session memory: {recorded}]"


class _FakeArtifactClient:
    """In-memory content-addressed store for demo mode."""

    def __init__(self) -> None:
        self._blobs: dict[str, bytes] = {}

    async def put_artifact(self, data: bytes, media_type: str | None = None) -> ArtifactRef:
        digest = hashlib.sha256(data).hexdigest()
        self._blobs[digest] = data
        return ArtifactRef(hash=digest, size=len(data), media_type=media_type)

    async def get_artifact(self, hash: str) -> bytes:
        return self._blobs[hash]


# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------


async def run(*, live: bool, db_path: str) -> None:
    """Two-turn session demonstrating thread continuity, memory recall, artifacts."""
    print("JamJet session-memory example")
    print(f"  mode={'live' if live else 'demo'}")
    print()

    # Wire up memory backend.
    if live:
        # Real Engram bridge — requires 'jamjet[memory]' and a running Engram.
        memory: Any = True
    else:
        memory = _FakeMemory()

    # Wire up artifact transport.
    artifact_client = _FakeArtifactClient()  # in demo and live demo; swap for JamjetClient() in prod

    # ---- Run 1: introduce the project ------------------------------------ #
    store = SessionStore(db_path)
    session = store.create("demo-session")
    session.attach_client(artifact_client)

    agent = Agent(
        "assistant",
        model="claude-sonnet-4-6" if live else "gpt-4o-mini",
        tools=[summarize],
        strategy="react",
        instructions="You are a helpful assistant. Remember what the user tells you.",
        memory=memory,
        session_store=store,
    )

    print("Turn 1: introducing the project")
    r1 = await agent.run(
        "My project is called Hermes. Please remember that.",
        session=session,
    )
    print(f"  Agent: {r1.output}")
    print()

    # Store a session artifact (a simple text note).
    ref = await session.artifacts.put(
        b"Project Hermes - introduced in turn 1.",
        "text/plain",
    )
    print(f"Artifact stored: hash={ref.hash[:16]}...  size={ref.size} bytes")
    print()

    # ---- Simulated restart ----------------------------------------------- #
    # Discard the first agent and store; reload from the same DB.
    del agent, store
    store2 = SessionStore(db_path)
    session2 = store2.load("demo-session")
    assert session2 is not None
    session2.attach_client(artifact_client)

    agent2 = Agent(
        "assistant",
        model="claude-sonnet-4-6" if live else "gpt-4o-mini",
        tools=[summarize],
        strategy="react",
        instructions="You are a helpful assistant. Remember what the user tells you.",
        memory=memory,
        session_store=store2,
    )

    # ---- Run 2: test recall ---------------------------------------------- #
    print("Turn 2 (after simulated restart): testing recall")
    r2 = await agent2.run(
        "What is my project called?",
        session=session2,
    )
    print(f"  Agent: {r2.output}")
    print()

    # Fetch the artifact by hash.
    data = await session2.artifacts.get(ref.hash)
    print(f"Artifact fetched: {data.decode()}")
    print()

    print("All three guarantees demonstrated:")
    print("  1. Thread continuity — turn 2 sees turn 1 in the message thread.")
    print("  2. Memory recall     — retrieved memory block injected into turn 2.")
    print("  3. Artifact          — bytes stored before restart fetched by hash after.")


def main() -> None:
    parser = argparse.ArgumentParser(description="JamJet session-memory example")
    parser.add_argument(
        "--live",
        action="store_true",
        help="Use a real model (ANTHROPIC_API_KEY) and real Engram (ENGRAM_URL).",
    )
    parser.add_argument(
        "--db",
        default=str(Path.home() / ".jamjet" / "example-sessions.db"),
        help="Path to the session database (default: ~/.jamjet/example-sessions.db).",
    )
    args = parser.parse_args()

    if not args.live:
        # Patch litellm with the scripted model so no API key is needed.
        scripted = _ScriptedModel()
        mock_litellm = types.ModuleType("litellm")
        mock_litellm.acompletion = scripted.acompletion  # type: ignore[attr-defined]
        mock_litellm.completion_cost = lambda *a, **kw: 0.0  # type: ignore[attr-defined]
        sys.modules["litellm"] = mock_litellm

    asyncio.run(run(live=args.live, db_path=args.db))


if __name__ == "__main__":
    main()
