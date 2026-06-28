"""Specialists + a router + the team factories for the multi-agent example.

This module is imported by BOTH the runner (``main.py``) and (on the durable
path) the ``jamjet worker`` that executes the ``@tool`` functions, so the tools
and the Agent factories live HERE, not in ``__main__`` — the compiled agent-loop
IR records each tool as ``{name: "specialists:<fn>"}`` (module + qualname), and
the worker resolves it by importing this module.

The team is pure Python orchestration over the single-agent path (Path A): each
sub-agent runs as its OWN execution via ``Agent.run`` / ``Agent.run_durable`` and
the team composes the results. No custom orchestration code, no Rust.
"""

from __future__ import annotations

from jamjet import Agent, Sequential, Team, tool

_MODEL = "anthropic/claude-sonnet-4-6"


# ── Tools ──────────────────────────────────────────────────────────────────────


@tool
async def web_search(query: str) -> str:
    """Look up background facts for a topic (stubbed for the example)."""
    return f"Background facts about: {query}"


@tool
async def word_count(text: str) -> str:
    """Count the words in a piece of text."""
    return str(len(text.split()))


# ── Specialists ─────────────────────────────────────────────────────────────────


def build_researcher() -> Agent:
    """A fact-finder: searches, then summarizes what it found."""
    return Agent(
        "researcher",
        model=_MODEL,
        tools=[web_search],
        instructions="You are a researcher. Use web_search to gather facts, then give a tight factual summary.",
        strategy="react",
    )


def build_writer() -> Agent:
    """A drafter: turns notes into a short, clear piece of writing."""
    return Agent(
        "writer",
        model=_MODEL,
        tools=[word_count],
        instructions="You are a writer. Turn the input into a short, clear paragraph. Keep it under 80 words.",
        strategy="react",
    )


def build_router() -> Agent:
    """A dispatcher whose OUTPUT names the specialist to run.

    The coordinator Team matches the router's output against the specialist names,
    so the router is instructed to answer with exactly one name.
    """
    return Agent(
        "router",
        model=_MODEL,
        tools=[],
        instructions=(
            "You are a dispatcher for a content desk. Reply with EXACTLY one word and nothing else: "
            "'researcher' if the request needs fact-finding, or 'writer' if it needs drafting or editing."
        ),
    )


# ── Teams ────────────────────────────────────────────────────────────────────────


def build_desk() -> Team:
    """A coordinator team: the router picks ONE specialist to handle the request.

    The team carries a governance default (a per-sub-agent budget cap). Because the
    specialists set no budget of their own, they INHERIT this cap and each enforces
    it — a team never bypasses governance.
    """
    return Team(
        [build_researcher(), build_writer()],
        coordinator=build_router(),
        name="content-desk",
        governance={"budget": 0.50},
    )


def build_pipeline() -> Sequential:
    """A sequential pipeline: research first, then write up the findings.

    The researcher's output is fed straight into the writer as its input.
    """
    return Sequential([build_researcher(), build_writer()], name="research-then-write")
