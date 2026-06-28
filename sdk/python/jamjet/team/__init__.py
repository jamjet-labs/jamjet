"""Team — multi-agent composition for JamJet (Track 6, Path A).

Compose several :class:`~jamjet.agents.agent.Agent` objects into a coordinated
multi-agent workflow. Each sub-agent runs as its OWN independent execution over
the already-shipped single-agent path (:meth:`Agent.run` / :meth:`Agent.run_durable`)
— Python orchestration, zero Rust.

    from jamjet import Sequential, Parallel, Team, Loop

    pipeline = Sequential([researcher, writer])
    result = await pipeline.run("Write a brief on agent runtimes")

    fanout = Parallel([a, b, c], merge="collect")
    desk = Team([researcher, writer], coordinator=router)
"""

from __future__ import annotations

from jamjet.team.team import (
    Collect,
    Custom,
    First,
    Loop,
    MergeStrategy,
    Parallel,
    Sequential,
    Team,
    TeamResult,
    derive_child_session,
)

__all__ = [
    "Collect",
    "Custom",
    "First",
    "Loop",
    "MergeStrategy",
    "Parallel",
    "Sequential",
    "Team",
    "TeamResult",
    "derive_child_session",
]
