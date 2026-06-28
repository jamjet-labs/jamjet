"""Compile a Team to per-sub-agent IRs + a composition plan (Track 6, Path A).

A ``Team`` is **N independent agent IRs orchestrated in Python**, NOT one fused
multi-agent IR. Each sub-agent compiles via the already-shipped single-agent
:func:`~jamjet.compiler.agent_ir.compile_agent_to_ir` (Track 2j), so each carries
its OWN governance (budget / policy / PII) and the durable run path can start one
execution per sub-agent. We deliberately do NOT emit the in-IR
``coordinator`` / ``agent_tool`` / ``subgraph`` node kinds — they are unwired in
the running runtime (a silent no-op stub) per the track grounding.

:func:`compile_team_to_ir` returns a small :class:`TeamPlan` describing the
composition (the ordered per-sub-agent IRs, the router IR when the coordinator is
an Agent, and the merge name for a parallel fan-out) — the plan the Python
orchestrator in :mod:`jamjet.team` executes.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from jamjet.compiler.agent_ir import compile_agent_to_ir
from jamjet.team.team import Collect, Custom, First, MergeStrategy, Parallel, Team

if TYPE_CHECKING:
    from jamjet.agents.agent import Agent
    from jamjet.team.team import _TeamBase

# The prompt is per-run and private; compile_agent_to_ir does NOT embed it in the
# IR (it only seeds build_initial_state), so a placeholder is correct here. Each
# sub-agent's real prompt is supplied at run time by the orchestrator.
_PLAN_PROMPT = ""


@dataclass
class CompiledSubAgent:
    """One sub-agent and its compiled durable agent-loop IR."""

    agent: Agent
    ir: dict[str, Any]


@dataclass
class TeamPlan:
    """The composition plan for a team: the per-sub-agent IRs + how to combine them.

    Attributes:
        pattern: ``"sequential"`` | ``"parallel"`` | ``"coordinator"`` | ``"loop"``.
        sub_agents: the specialist sub-agents, each with its OWN compiled IR, in
            declared order (the sequential pipeline order / the parallel fan-out set
            / the coordinator's candidate specialists / the single looped agent).
        coordinator: the router's :class:`CompiledSubAgent` when the team's
            coordinator is itself an Agent; ``None`` for a routing-callable
            coordinator or a non-coordinator pattern.
        merge: the merge-strategy name (``"collect"`` / ``"first"`` / ``"custom"``)
            for a parallel team; ``None`` otherwise.
    """

    pattern: str
    sub_agents: list[CompiledSubAgent]
    coordinator: CompiledSubAgent | None = None
    merge: str | None = None

    @property
    def irs(self) -> list[dict[str, Any]]:
        """Every sub-agent's IR, in order — the N independent IRs of the team."""
        return [c.ir for c in self.sub_agents]


def merge_name(merge: MergeStrategy) -> str:
    """A stable name for a :class:`~jamjet.team.team.MergeStrategy` instance."""
    if isinstance(merge, Collect):
        return "collect"
    if isinstance(merge, First):
        return "first"
    if isinstance(merge, Custom):
        return "custom"
    return type(merge).__name__.lower()


def compile_team_to_ir(team: _TeamBase, max_turns: int = 8) -> TeamPlan:
    """Compile *team* into a :class:`TeamPlan` of per-sub-agent IRs.

    Each sub-agent is compiled independently with
    :func:`~jamjet.compiler.agent_ir.compile_agent_to_ir`, so each IR carries its
    own governance and is a complete, runnable durable agent-loop — the team is the
    Python composition OVER these N IRs, never a single merged IR.

    Parameters
    ----------
    team:
        A :class:`~jamjet.team.team.Sequential` / ``Parallel`` / ``Team`` / ``Loop``.
    max_turns:
        Static-unroll bound forwarded to each sub-agent's compile.
    """
    sub_agents = [
        CompiledSubAgent(agent=agent, ir=compile_agent_to_ir(agent, _PLAN_PROMPT, max_turns)) for agent in team.agents
    ]

    coordinator: CompiledSubAgent | None = None
    if isinstance(team, Team) and _is_agent(team.coordinator):
        coordinator = CompiledSubAgent(
            agent=team.coordinator,  # type: ignore[arg-type]
            ir=compile_agent_to_ir(team.coordinator, _PLAN_PROMPT, max_turns),  # type: ignore[arg-type]
        )

    merge: str | None = None
    if isinstance(team, Parallel):
        merge = merge_name(team.merge)

    return TeamPlan(
        pattern=team.pattern,
        sub_agents=sub_agents,
        coordinator=coordinator,
        merge=merge,
    )


def _is_agent(obj: Any) -> bool:
    if obj is None:
        return False
    from jamjet.agents.agent import Agent  # noqa: PLC0415 - avoid import cycle

    return isinstance(obj, Agent)


__all__ = ["CompiledSubAgent", "TeamPlan", "compile_team_to_ir", "merge_name"]
