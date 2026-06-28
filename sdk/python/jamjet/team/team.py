"""Team — friendly multi-agent composition over the single-agent durable path.

Track 6 (Path A). A ``Team`` composes several :class:`~jamjet.agents.agent.Agent`
objects into a coordinated multi-agent workflow WITHOUT writing orchestration.
The load-bearing decision: **Python orchestration over the already-working
single-agent path**. Each sub-agent runs as its OWN independent execution via the
shipped :meth:`Agent.run` (in-process) / :meth:`Agent.run_durable` (durable);
a team simply composes N such runs in Python. We do NOT touch Rust and we do NOT
use the in-IR ``coordinator`` / ``agent_tool`` / ``subgraph`` nodes (they hit a
silent no-op stub in the running runtime — see the track grounding).

Four patterns, each a thin Python orchestrator over that primitive:

- :class:`Sequential` — a pipeline ``a -> b -> c``; each agent's output is the
  next agent's input. Halts on the first failing step (a broken upstream output
  cannot sensibly feed downstream); the error lands in ``TeamResult.per_agent``.
- :class:`Parallel` — fan the SAME input out to every agent concurrently
  (``asyncio.gather(..., return_exceptions=True)``), then aggregate via a
  :class:`MergeStrategy`. A failing sub-agent is isolated (its error is recorded;
  siblings are unaffected).
- :class:`Team` — a coordinator routes the input to ONE specialist. The
  coordinator is either a *router* :class:`~jamjet.agents.agent.Agent` (its output
  names the specialist) or a plain Python routing callable.
- :class:`Loop` — re-run a single agent until a predicate holds (or a max-iters
  bound), threading each output into the next iteration (a refinement loop).

Every pattern returns a common :class:`TeamResult` carrying the combined
``output``, the per-sub-agent results/errors, the ``pattern`` name, and whether
the run was ``durable``.

Governance + sessions (T6-5)
----------------------------
Each sub-agent compiles and enforces its OWN governance (budget / PII / policy /
allowlist); a team adds no enforcement layer, so a sub-agent's own explicit
governance is never bypassed. A team may carry a ``governance=`` default applied
ONLY to a sub-agent that set no explicit governance (all-default); it never
overrides a sub-agent's own explicit knob, so a governed sub-agent is never
weakened by the team (explicit beats inherited). The default is not a
tighten-only knob: applied to an all-default sub-agent it may turn a
framework-default-on protection (such as PII) off, which is the team author's
deliberate choice, not a bypass. A team may carry a ``session=`` that namespaces a
per-sub-agent child session so concurrent sub-agents never race on one SQLite row
(see :func:`derive_child_session`).
"""

from __future__ import annotations

import asyncio
import inspect
import re
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from jamjet.agents.governance import GovernanceConfig, normalize_governance
from jamjet.agents.session import Session, SessionStore

if TYPE_CHECKING:
    from jamjet.agents.agent import Agent, AgentResult

# A sub-agent run can succeed (AgentResult) or fail (the exception is captured so
# one failing sub-agent never crashes the team — child-crash isolation).
PerAgent = dict[str, "AgentResult | BaseException"]

# Default durable engine URL, matching Agent.run_durable's default.
_DEFAULT_RUNTIME_URL = "http://127.0.0.1:7700"


# ── Merge strategies (the parallel aggregation vocabulary) ─────────────────────


class MergeStrategy:
    """How a :class:`Parallel` team combines its sub-agents' results into one
    ``output`` string. Subclasses implement :meth:`merge` over the ordered
    per-agent mapping (insertion order == declared agent order)."""

    def merge(self, per_agent: PerAgent) -> str:  # pragma: no cover - abstract
        raise NotImplementedError


class Collect(MergeStrategy):
    """Combine every SUCCESSFUL output into a labeled, newline-joined block.

    Failures (captured exceptions) are skipped. The result is ``"[name] output"``
    lines in declared agent order — a readable digest of the fan-out.
    """

    def merge(self, per_agent: PerAgent) -> str:
        parts: list[str] = []
        for name, result in per_agent.items():
            if isinstance(result, BaseException):
                continue
            parts.append(f"[{name}] {result.output}")
        return "\n".join(parts)


class First(MergeStrategy):
    """The first SUCCESSFUL sub-agent's output, in declared agent order.

    Deterministic: because the parallel pattern gathers ALL results (for
    child-crash isolation), "first" means the first agent in declaration order
    that succeeded, not the first to finish on the wall clock (a true race +
    cancel is a follow-up). Returns ``""`` when every sub-agent failed.
    """

    def merge(self, per_agent: PerAgent) -> str:
        for result in per_agent.values():
            if not isinstance(result, BaseException):
                return result.output
        return ""


class Custom(MergeStrategy):
    """Aggregate via a caller-supplied callable over the per-agent mapping.

    The callable receives the ordered ``{name: AgentResult | exception}`` dict and
    returns the combined ``output`` string.
    """

    def __init__(self, fn: Callable[[PerAgent], str]) -> None:
        self._fn = fn

    def merge(self, per_agent: PerAgent) -> str:
        return self._fn(per_agent)


def _resolve_merge(merge: MergeStrategy | Callable[[PerAgent], str] | str | None) -> MergeStrategy:
    """Coerce the ``merge=`` argument into a :class:`MergeStrategy`."""
    if merge is None:
        return Collect()
    if isinstance(merge, MergeStrategy):
        return merge
    if isinstance(merge, str):
        key = merge.strip().lower()
        if key == "collect":
            return Collect()
        if key == "first":
            return First()
        raise ValueError(f"unknown merge strategy {merge!r}; use 'collect', 'first', a MergeStrategy, or a callable")
    if callable(merge):
        return Custom(merge)
    raise TypeError(f"merge must be a MergeStrategy, a callable, or a string — got {type(merge).__name__!r}")


# ── The combined result ────────────────────────────────────────────────────────


@dataclass
class TeamResult:
    """The result of running a team.

    Attributes:
        output: The combined output (the merged string for parallel, the last
            successful output for sequential / loop, the chosen specialist's
            output for a coordinator).
        per_agent: Per-sub-agent result keyed by name. A value is an
            :class:`~jamjet.agents.agent.AgentResult` on success or the captured
            exception on failure — a failing sub-agent is isolated here, never a
            team crash. Keys preserve run order.
        pattern: ``"sequential"`` | ``"parallel"`` | ``"coordinator"`` | ``"loop"``.
        durable: ``True`` when each sub-agent ran as a durable execution
            (:meth:`Team.run_durable`); ``False`` for the in-process
            :meth:`Team.run`.
    """

    output: str
    per_agent: PerAgent
    pattern: str
    durable: bool

    def __str__(self) -> str:
        return self.output

    @property
    def errors(self) -> dict[str, BaseException]:
        """The subset of ``per_agent`` whose runs failed (exception values)."""
        return {name: r for name, r in self.per_agent.items() if isinstance(r, BaseException)}

    @property
    def ok(self) -> bool:
        """True when no sub-agent failed."""
        return not self.errors


# ── Session derivation (race-safe per-sub-agent threads) ───────────────────────


def derive_child_session(
    team_session: Session | str | None,
    index: int,
    agent_name: str,
) -> Session | None:
    """Derive a per-sub-agent child :class:`Session` from a team-level session.

    Returns ``None`` when the team carries no session (sub-agents run stateless).

    Each sub-agent gets its OWN session id ``"{team_id}::{index}-{name}"`` so a
    sub-agent has a persistent, independent thread that survives across team runs
    WITHOUT racing siblings. This matters because :class:`SessionStore` is
    single-writer-per-id (last-writer-wins): the parallel pattern runs siblings
    concurrently, so they MUST write distinct rows. The ``index`` keeps ids unique
    even when two sub-agents share a name. The child is loaded get-or-create from
    the team session's originating store (or a default store), so threads persist.
    """
    if team_session is None:
        return None
    if isinstance(team_session, Session):
        base_id = team_session.id
        store = getattr(team_session, "_store", None) or SessionStore()
    else:
        base_id = team_session
        store = SessionStore()
    child_id = f"{base_id}::{index}-{agent_name}"
    return store.create(child_id)


# ── Governance inheritance ──────────────────────────────────────────────────────


def _resolve_team_governance(governance: GovernanceConfig | dict | None) -> GovernanceConfig | None:
    """Coerce the team's ``governance=`` default into a :class:`GovernanceConfig`."""
    if governance is None:
        return None
    if isinstance(governance, GovernanceConfig):
        return governance
    if isinstance(governance, dict):
        return normalize_governance(**governance)
    raise TypeError(f"governance must be a GovernanceConfig, dict, or None — got {type(governance).__name__!r}")


# The all-default sentinel: a sub-agent whose governance equals this set NO
# explicit governance knob, so a team default may be inherited into it.
_DEFAULT_GOVERNANCE = GovernanceConfig()


def _apply_governance_default(agents: list[Agent], team_governance: GovernanceConfig | None) -> None:
    """Apply the team governance default to all-default sub-agents (in place).

    A sub-agent whose ``governance`` is the all-default sentinel set no explicit
    knob, so it INHERITS the team default (its compiled IR then carries the team's
    budget / policy / PII and enforces it). A sub-agent that set ANY governance
    knob keeps its own config wholesale (explicit beats inherited), so a governed
    sub-agent is never overridden or weakened by the team. The default is applied
    ONLY to an all-default sub-agent and NEVER overrides a sub-agent's own explicit
    knob; it does not only tighten, since for an all-default sub-agent a team
    default such as ``pii=False`` turns a framework-default-on protection off (the
    team author's deliberate choice, not a bypass). This mutates the sub-agent's
    ``governance`` attribute; it only happens when the team carries a default AND
    the sub-agent was all-default.
    """
    if team_governance is None:
        return
    for agent in agents:
        if agent.governance == _DEFAULT_GOVERNANCE:
            agent.governance = team_governance


# ── The base team ──────────────────────────────────────────────────────────────


class _TeamBase:
    """Shared machinery for the four patterns: governance/session inheritance,
    the per-sub-agent run primitive, and the ``run`` / ``run_durable`` entry
    points (each delegating to the subclass's :meth:`_orchestrate`)."""

    pattern: str = "team"

    def __init__(
        self,
        agents: list[Agent],
        *,
        name: str = "team",
        governance: GovernanceConfig | dict | None = None,
        session: Session | str | None = None,
    ) -> None:
        if not agents:
            raise ValueError("a team needs at least one agent")
        self.agents = list(agents)
        # Names must be unique: per_agent keys by agent.name and the coordinator
        # routes by name, so two members sharing a name would silently overwrite a
        # result and make routing ambiguous. Catch it at construction.
        seen: set[str] = set()
        dupes: set[str] = set()
        for agent in self.agents:
            if agent.name in seen:
                dupes.add(agent.name)
            seen.add(agent.name)
        if dupes:
            raise ValueError(f"team members must have unique names; duplicates: {sorted(dupes)}")
        self.name = name
        self.session = session
        self.governance = _resolve_team_governance(governance)
        # T6-5: push the team governance default into all-default sub-agents so
        # each sub-agent compiles + enforces it (explicit knobs always win; the
        # default never overrides a sub-agent's own governance).
        _apply_governance_default(self.agents, self.governance)

    async def run(self, input: str) -> TeamResult:
        """Run the team in-process (each sub-agent via :meth:`Agent.run`)."""
        return await self._orchestrate(input, durable=False, runtime_url=_DEFAULT_RUNTIME_URL)

    async def run_durable(self, input: str, *, runtime_url: str = _DEFAULT_RUNTIME_URL) -> TeamResult:
        """Run the team durably (each sub-agent its own :meth:`Agent.run_durable`
        execution, polled to a terminal state)."""
        return await self._orchestrate(input, durable=True, runtime_url=runtime_url)

    # -- the per-sub-agent run primitive --------------------------------------

    async def _run_one(
        self,
        agent: Agent,
        prompt: str,
        *,
        index: int,
        durable: bool,
        runtime_url: str,
    ) -> AgentResult:
        """Run a single sub-agent as its own independent execution.

        In-process (:meth:`Agent.run`) or durable (:meth:`Agent.run_durable`),
        matching the team's run mode. When the team carries a session, the
        sub-agent runs with a derived per-sub-agent child session so siblings
        never race. Exceptions propagate to the caller, which isolates them.
        """
        child = derive_child_session(self.session, index, agent.name)
        if durable:
            return await agent.run_durable(prompt, runtime_url=runtime_url, session=child)
        return await agent.run(prompt, session=child)

    async def _orchestrate(self, input: str, *, durable: bool, runtime_url: str) -> TeamResult:  # pragma: no cover
        raise NotImplementedError("subclasses implement the pattern orchestration")


# ── Sequential ──────────────────────────────────────────────────────────────────


class Sequential(_TeamBase):
    """A pipeline: each agent's output is the next agent's input.

    ``Sequential([a, b, c]).run("go")`` runs ``a("go") -> b(a.out) -> c(b.out)``
    and returns the last agent's output. Halts on the first failing step (a broken
    upstream output cannot feed downstream); the failure is recorded in
    ``TeamResult.per_agent`` and no later agent runs.
    """

    pattern = "sequential"

    def __init__(
        self,
        agents: list[Agent],
        *,
        name: str = "sequential",
        governance: GovernanceConfig | dict | None = None,
        session: Session | str | None = None,
    ) -> None:
        super().__init__(agents, name=name, governance=governance, session=session)

    async def _orchestrate(self, input: str, *, durable: bool, runtime_url: str) -> TeamResult:
        per_agent: PerAgent = {}
        current = input
        last_output = ""
        for i, agent in enumerate(self.agents):
            try:
                result = await self._run_one(agent, current, index=i, durable=durable, runtime_url=runtime_url)
            except Exception as exc:  # noqa: BLE001 - isolate any sub-agent failure
                per_agent[agent.name] = exc
                break  # halt-on-error: do not feed a broken output downstream
            per_agent[agent.name] = result
            last_output = result.output
            current = result.output  # thread the output forward
        return TeamResult(output=last_output, per_agent=per_agent, pattern=self.pattern, durable=durable)


# ── Parallel ────────────────────────────────────────────────────────────────────


class Parallel(_TeamBase):
    """Fan the same input out to every agent concurrently, then aggregate.

    ``Parallel([a, b], merge=Collect()).run("go")`` runs ``a("go")`` and
    ``b("go")`` concurrently (``asyncio.gather(..., return_exceptions=True)``) and
    merges the results. A failing sub-agent is isolated — its exception is captured
    in ``per_agent`` and siblings are unaffected.
    """

    pattern = "parallel"

    def __init__(
        self,
        agents: list[Agent],
        *,
        merge: MergeStrategy | Callable[[PerAgent], str] | str | None = None,
        name: str = "parallel",
        governance: GovernanceConfig | dict | None = None,
        session: Session | str | None = None,
    ) -> None:
        super().__init__(agents, name=name, governance=governance, session=session)
        self.merge = _resolve_merge(merge)

    async def _orchestrate(self, input: str, *, durable: bool, runtime_url: str) -> TeamResult:
        tasks = [
            self._run_one(agent, input, index=i, durable=durable, runtime_url=runtime_url)
            for i, agent in enumerate(self.agents)
        ]
        # return_exceptions=True => one failed child never corrupts siblings; each
        # sub-agent is its own execution, so isolation is natural under Path A.
        results = await asyncio.gather(*tasks, return_exceptions=True)
        per_agent: PerAgent = {agent.name: result for agent, result in zip(self.agents, results)}
        output = self.merge.merge(per_agent)
        return TeamResult(output=output, per_agent=per_agent, pattern=self.pattern, durable=durable)


# ── Team (coordinator / router) ─────────────────────────────────────────────────

# A routing callable: given the input and the candidate agents, return the chosen
# Agent, its name, or its index.
RoutingFn = Callable[[str, "list[Agent]"], "Agent | str | int | Awaitable[Agent | str | int]"]


class Team(_TeamBase):
    """A coordinator routes the input to ONE specialist, then that one runs.

    The ``coordinator`` is either:

    - a *router* :class:`~jamjet.agents.agent.Agent` whose OUTPUT names the chosen
      specialist (matched against the sub-agents by name), or
    - a Python routing callable ``(input, agents) -> Agent | name | index`` (may be
      async), or
    - ``None`` — routes to the first agent (a single-specialist degenerate team).

    The router (when an Agent) and the chosen specialist both appear in
    ``per_agent``; ``output`` is the specialist's output.
    """

    pattern = "coordinator"

    def __init__(
        self,
        agents: list[Agent],
        *,
        coordinator: Agent | RoutingFn | None = None,
        name: str = "team",
        governance: GovernanceConfig | dict | None = None,
        session: Session | str | None = None,
    ) -> None:
        super().__init__(agents, name=name, governance=governance, session=session)
        self.coordinator = coordinator

    async def _orchestrate(self, input: str, *, durable: bool, runtime_url: str) -> TeamResult:
        per_agent: PerAgent = {}
        specialist = await self._route(input, per_agent, durable=durable, runtime_url=runtime_url)
        if specialist is None:
            # Routing itself failed (the router crashed); the error is already in
            # per_agent. Return an empty combined output rather than crashing.
            return TeamResult(output="", per_agent=per_agent, pattern=self.pattern, durable=durable)

        # The specialist's index in self.agents keeps its child-session id stable.
        idx = self.agents.index(specialist)
        try:
            result = await self._run_one(specialist, input, index=idx, durable=durable, runtime_url=runtime_url)
        except Exception as exc:  # noqa: BLE001 - isolate the specialist failure
            per_agent[specialist.name] = exc
            return TeamResult(output="", per_agent=per_agent, pattern=self.pattern, durable=durable)
        per_agent[specialist.name] = result
        return TeamResult(output=result.output, per_agent=per_agent, pattern=self.pattern, durable=durable)

    async def _route(
        self,
        input: str,
        per_agent: PerAgent,
        *,
        durable: bool,
        runtime_url: str,
    ) -> Agent | None:
        """Pick the specialist. Records a router-Agent's result in ``per_agent``.

        Returns the chosen :class:`Agent`, or ``None`` if routing crashed.
        """
        coord = self.coordinator
        if coord is None:
            return self.agents[0]

        # A router Agent: run it, then match its output to a specialist by name.
        if _is_agent(coord):
            # The router runs at index -1 so its derived child session never
            # collides with a specialist's (which use 0..n-1).
            try:
                router_result = await self._run_one(coord, input, index=-1, durable=durable, runtime_url=runtime_url)
            except Exception as exc:  # noqa: BLE001 - isolate router failure
                per_agent[coord.name] = exc
                return None
            per_agent[coord.name] = router_result
            return self._match_specialist(router_result.output)

        # A routing callable: (input, agents) -> Agent | name | index (maybe async).
        # Coercion runs INSIDE the try so a misuse (unknown name, out-of-range
        # index, foreign Agent) becomes an ISOLATED recorded error and a clean
        # empty TeamResult, not an opaque team crash.
        try:
            choice = coord(input, self.agents)
            # Await ANY awaitable (a coroutine, a Future/Task, or a custom
            # __await__), not just a coroutine, before coercing the choice.
            if inspect.isawaitable(choice):
                choice = await choice
            return self._coerce_choice(choice)
        except Exception as exc:  # noqa: BLE001 - isolate routing-callable failure
            per_agent[f"{self.name}:coordinator"] = exc
            return None

    def _match_specialist(self, router_output: str) -> Agent:
        """Match a router's free-text output to a specialist by name.

        Exact name match wins; otherwise the first agent whose name appears as a
        WHOLE WORD (case-insensitive, word-boundary) in the output; otherwise the
        first agent (a safe, documented fallback so an ambiguous router never
        crashes the team). The word-boundary pass avoids mis-routing overlapping
        names: output "rewriter" matches ``rewriter`` and not the substring
        ``writer``.
        """
        text = (router_output or "").strip()
        for agent in self.agents:
            if agent.name == text:
                return agent
        low = text.lower()
        for agent in self.agents:
            if re.search(rf"\b{re.escape(agent.name.lower())}\b", low):
                return agent
        return self.agents[0]

    def _coerce_choice(self, choice: Agent | str | int) -> Agent:
        """Coerce a routing callable's return (Agent / name / index) to an Agent.

        A misuse (a foreign Agent, an out-of-range index, or an unknown name)
        raises a clear ValueError/TypeError; the caller (_route) catches it so the
        error is isolated and recorded rather than crashing the team opaquely.
        """
        if _is_agent(choice):
            if choice not in self.agents:
                raise ValueError("coordinator returned an Agent that is not a member of this team")
            return choice  # type: ignore[return-value]
        if isinstance(choice, int):
            try:
                return self.agents[choice]
            except IndexError:
                raise ValueError(
                    f"coordinator returned out-of-range agent index {choice}; team has {len(self.agents)} agents"
                ) from None
        if isinstance(choice, str):
            for agent in self.agents:
                if agent.name == choice:
                    return agent
            raise ValueError(f"coordinator returned unknown agent name {choice!r}")
        raise TypeError(f"coordinator must return an Agent, a name, or an index — got {type(choice).__name__!r}")


# ── Loop ─────────────────────────────────────────────────────────────────────────

# A loop predicate over the latest AgentResult: True => stop.
LoopPredicate = Callable[["AgentResult"], bool]


class Loop(_TeamBase):
    """Re-run a single agent until a predicate holds (or a max-iters bound).

    ``Loop(agent, until=is_done, max_iters=5).run("draft")`` runs the agent on the
    input, then re-runs it on each previous output (a refinement loop), stopping
    when ``until(result)`` is truthy or after ``max_iters`` iterations. Each
    iteration is keyed ``"name#i"`` in ``per_agent``; ``output`` is the last
    iteration's output. A failing iteration is isolated and stops the loop.
    """

    pattern = "loop"

    def __init__(
        self,
        agent: Agent,
        *,
        until: LoopPredicate | None = None,
        max_iters: int = 5,
        name: str = "loop",
        governance: GovernanceConfig | dict | None = None,
        session: Session | str | None = None,
    ) -> None:
        if max_iters < 1:
            raise ValueError("max_iters must be >= 1")
        super().__init__([agent], name=name, governance=governance, session=session)
        self.agent = agent
        self.until = until
        self.max_iters = max_iters

    async def _orchestrate(self, input: str, *, durable: bool, runtime_url: str) -> TeamResult:
        per_agent: PerAgent = {}
        current = input
        last_output = ""
        for i in range(self.max_iters):
            try:
                result = await self._run_one(self.agent, current, index=0, durable=durable, runtime_url=runtime_url)
            except Exception as exc:  # noqa: BLE001 - isolate the iteration failure
                per_agent[f"{self.agent.name}#{i}"] = exc
                break
            per_agent[f"{self.agent.name}#{i}"] = result
            last_output = result.output
            current = result.output
            if self.until is not None and self.until(result):
                break
        return TeamResult(output=last_output, per_agent=per_agent, pattern=self.pattern, durable=durable)


# ── small helpers ────────────────────────────────────────────────────────────────


def _is_agent(obj: Any) -> bool:
    """True if *obj* is a :class:`~jamjet.agents.agent.Agent` (imported lazily to
    avoid an import cycle: agent.py does not import team, and team is optional)."""
    from jamjet.agents.agent import Agent  # noqa: PLC0415

    return isinstance(obj, Agent)


__all__ = [
    "Collect",
    "Custom",
    "First",
    "Loop",
    "MergeStrategy",
    "Parallel",
    "RoutingFn",
    "Sequential",
    "Team",
    "TeamResult",
    "derive_child_session",
]
