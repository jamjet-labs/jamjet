"""
TrajectoryScorer -- deterministic assertions over an agent run's step sequence.

A Trajectory is the ordered list of steps (tools called, nodes, turns) an agent
took during a run. TrajectoryScorer scores a trajectory against an
expected-trajectory spec using deterministic structural assertions with no LLM
calls in the default path.

Trajectory sources
------------------
- ``Trajectory.from_agent_result(result)`` -- in-process AgentResult.tool_calls
- ``Trajectory.from_events(events)`` -- event log from get_events() (ToolCalled events)

Both sources produce the same Trajectory shape; the tool_sequence and step_count
are identical for the same run.

Spec keys (all optional -- only keys present are checked)
---------------------------------------------------------
- ``expected_tools``: list[str] -- all these tools must appear (subset check)
- ``expected_tools_exact``: bool -- if True, tool set must match exactly (default: False)
- ``tool_sequence``: list[str] -- tools must appear in this order (ordered subsequence)
- ``used_tool``: str | list[str] -- each named tool must appear
- ``did_not_use``: str | list[str] -- each named tool must NOT appear
- ``max_turns``: int -- step count must be <= this value
- ``step_count``: int -- step count must equal this value exactly

Determinism guarantee
---------------------
The score() method is a pure function of the trajectory and the expected spec.
Same trajectory + same spec -> identical ScorerResult, always. No wall-clock,
random, or external state is read in the deterministic path.

Optional LLM judge
------------------
Pass ``judge=True`` at construction to also run an LLM-as-judge over the rendered
trajectory trace. This is NON-DETERMINISTIC and OFF by default. The judge result
is appended to the message but does NOT override the structural pass/fail. Enable
only when you understand and accept the non-determinism.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from jamjet.eval.scorers import BaseScorer, ScorerResult

if TYPE_CHECKING:
    pass


@dataclass
class TrajectoryStep:
    """A single step in an agent trajectory."""

    tool: str | None = None
    node_id: str | None = None
    args: dict[str, Any] = field(default_factory=dict)
    output: Any = None


@dataclass
class TrajectoryAssertionResult:
    """Result of a single named trajectory assertion."""

    name: str
    passed: bool
    message: str


@dataclass
class TrajectoryResult(ScorerResult):
    """ScorerResult extended with a per-assertion breakdown.

    Fully compatible with ScorerResult -- all standard fields are present.
    The ``assertions`` list carries the fine-grained pass/fail detail per
    configured assertion.
    """

    assertions: list[TrajectoryAssertionResult] = field(default_factory=list)


class Trajectory:
    """Ordered sequence of steps (tool calls) in an agent run.

    Construct via the class methods rather than directly:

    - :meth:`from_agent_result` -- in-process Agent.run() result
    - :meth:`from_events` -- durable event log from get_events()
    """

    def __init__(self, steps: list[TrajectoryStep]) -> None:
        self._steps = list(steps)

    # ── Properties ───────────────────────────────────────────────────────────

    @property
    def steps(self) -> list[TrajectoryStep]:
        """Ordered list of all steps (copy)."""
        return list(self._steps)

    @property
    def tool_sequence(self) -> list[str]:
        """Ordered list of tool names called (steps without a tool name skipped)."""
        return [s.tool for s in self._steps if s.tool is not None]

    @property
    def tool_set(self) -> set[str]:
        """Set of distinct tool names used across all steps."""
        return set(self.tool_sequence)

    @property
    def step_count(self) -> int:
        """Total number of steps (including non-tool steps)."""
        return len(self._steps)

    # ── Constructors ──────────────────────────────────────────────────────────

    @classmethod
    def from_agent_result(cls, result: Any) -> Trajectory:
        """Build a Trajectory from AgentResult.tool_calls (in-process path).

        Each element of ``result.tool_calls`` is a dict with keys:
        ``tool``, ``input``, ``output``, ``duration_us`` (as produced by
        ``Agent._tool_calls_from_messages`` and in-process ToolCallRecord.model_dump()).
        """
        steps: list[TrajectoryStep] = []
        for tc in result.tool_calls or []:
            steps.append(
                TrajectoryStep(
                    tool=tc.get("tool"),
                    node_id=None,  # not available in-process
                    args=tc.get("input") or {},
                    output=tc.get("output"),
                )
            )
        return cls(steps)

    @classmethod
    def from_events(cls, events: list[dict[str, Any]] | dict[str, Any]) -> Trajectory:
        """Build a Trajectory from the durable event log (event log path).

        Accepts either:
        - A raw list of event dicts (the ``events`` list from get_events)
        - The wrapped ``{"events": [...]}`` dict returned by get_events()

        Only ``ToolCalled`` events (``kind.type == "tool_called"``) contribute
        steps; all other event kinds are ignored.
        """
        if isinstance(events, dict):
            event_list: list[dict[str, Any]] = events.get("events", [])
        else:
            event_list = events

        steps: list[TrajectoryStep] = []
        for evt in event_list:
            kind = evt.get("kind", {})
            if kind.get("type") == "tool_called":
                steps.append(
                    TrajectoryStep(
                        tool=kind.get("tool"),
                        node_id=kind.get("node_id"),
                        args={},
                        output=None,
                    )
                )
        return cls(steps)

    # ── Rendering ─────────────────────────────────────────────────────────────

    def render(self) -> str:
        """Human-readable trace for debugging or LLM judge input."""
        lines = [f"Trajectory ({self.step_count} steps):"]
        for i, step in enumerate(self._steps, 1):
            tool_part = step.tool or "(no tool)"
            node_part = f" [node={step.node_id}]" if step.node_id else ""
            lines.append(f"  {i}. {tool_part}{node_part}")
        return "\n".join(lines)

    def __repr__(self) -> str:
        return f"Trajectory(steps={self.step_count}, tools={self.tool_sequence!r})"


class TrajectoryScorer(BaseScorer):
    """Scores an agent run's trajectory with DETERMINISTIC structural assertions.

    Configured by an ``expected`` spec dict (see module docstring for all keys).
    Returns a :class:`TrajectoryResult` (a ScorerResult subclass) with overall
    pass/fail, a fractional score (passed_assertions / total_assertions), and a
    per-assertion breakdown in ``result.assertions``.

    All assertions are independent -- each is checked and reported separately.
    The overall result passes only when every configured assertion passes.

    DETERMINISM: the score is a pure function of the trajectory and the spec.
    Same inputs -> same outputs, always. No model calls are made by default.

    Optional LLM judge: pass ``judge=True`` to enable non-deterministic
    LLM-as-judge scoring over the rendered trace (see module docstring).

    Usage::

        scorer = TrajectoryScorer(expected={
            "tool_sequence": ["search", "calculate"],
            "did_not_use": "dangerous_tool",
            "max_turns": 5,
        })
        result = await scorer.score(output, trajectory=trajectory)
        assert result.passed          # overall
        for a in result.assertions:   # per-assertion breakdown
            print(a.name, a.passed, a.message)
    """

    name = "trajectory"

    def __init__(
        self,
        expected: dict[str, Any],
        *,
        judge: bool = False,
        judge_rubric: str = "Rate whether the agent followed an appropriate tool-use trajectory (1-5).",
        judge_model: str = "claude-haiku-4-5-20251001",
        judge_model_fn: Any | None = None,
    ) -> None:
        self.expected = expected
        self._judge_enabled = judge
        self._judge_rubric = judge_rubric
        self._judge_model = judge_model
        self._judge_model_fn = judge_model_fn

    async def score(  # type: ignore[override]
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
        trajectory: Trajectory | None = None,
    ) -> TrajectoryResult:
        """Score the trajectory against the configured expected-trajectory spec.

        Args:
            output: The agent's final output (passed through from scorer interface;
                not used for trajectory scoring but kept for interface compatibility).
            trajectory: The :class:`Trajectory` to score. If ``None`` (e.g. when
                called from an existing runner that does not yet pass a trajectory),
                all assertions are skipped and the result is passed with no breakdown.
            expected: Not used (trajectory spec is in ``self.expected``).
            duration_ms, cost_usd, input_data: Standard scorer kwargs; not used.

        Returns:
            :class:`TrajectoryResult` with ``passed``, ``score`` (fraction of
            assertions that passed), ``message``, and per-assertion ``assertions``.
            Scoring is deterministic unless ``judge=True`` was set at construction.
        """
        if trajectory is None:
            return TrajectoryResult(
                scorer=self.name,
                passed=True,
                score=None,
                message="no trajectory provided; skipping trajectory assertions",
                assertions=[],
            )

        assertions: list[TrajectoryAssertionResult] = []

        # Run each configured assertion in the order they appear in the spec.
        if "expected_tools" in self.expected:
            assertions.append(self._check_expected_tools(trajectory))

        if "tool_sequence" in self.expected:
            assertions.append(self._check_tool_sequence(trajectory))

        if "used_tool" in self.expected:
            assertions.append(self._check_used_tool(trajectory))

        if "did_not_use" in self.expected:
            assertions.append(self._check_did_not_use(trajectory))

        if "max_turns" in self.expected:
            assertions.append(self._check_max_turns(trajectory))

        if "step_count" in self.expected:
            assertions.append(self._check_step_count(trajectory))

        total = len(assertions)
        passed_count = sum(1 for a in assertions if a.passed)
        all_passed = passed_count == total
        score = float(passed_count) / total if total > 0 else 1.0

        if all_passed:
            message = f"all {total} assertion(s) passed"
        else:
            failed_names = [a.name for a in assertions if not a.passed]
            message = f"failed ({total - passed_count}/{total}): {', '.join(failed_names)}"

        # Optional LLM judge -- OFF by default; non-deterministic.
        # Gate is explicit: judge must be True at construction. When enabled, the
        # judge result is appended to the message for transparency but does NOT
        # override the structural pass/fail (which remains the source of truth).
        if self._judge_enabled:
            judge_result = await self._run_judge(trajectory)
            message += f"; judge: {judge_result.message}"

        return TrajectoryResult(
            scorer=self.name,
            passed=all_passed,
            score=score,
            message=message,
            assertions=assertions,
        )

    # ── Deterministic assertion implementations ───────────────────────────────

    def _check_expected_tools(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """All expected tools must appear (subset check by default; exact if flag set)."""
        expected_set = set(self.expected["expected_tools"])
        actual_set = trajectory.tool_set
        exact = bool(self.expected.get("expected_tools_exact", False))

        if exact:
            passed = actual_set == expected_set
            missing = sorted(expected_set - actual_set)
            extra = sorted(actual_set - expected_set)
            if passed:
                msg = f"tool set matches exactly: {sorted(expected_set)}"
            else:
                parts = []
                if missing:
                    parts.append(f"missing {missing}")
                if extra:
                    parts.append(f"unexpected {extra}")
                msg = f"tool set mismatch: {'; '.join(parts)}"
        else:
            missing = sorted(expected_set - actual_set)
            passed = not missing
            msg = f"all expected tools present: {sorted(expected_set)}" if passed else f"missing tools: {missing}"

        return TrajectoryAssertionResult(name="expected_tools", passed=passed, message=msg)

    def _check_tool_sequence(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """Tools must appear in the specified order (ordered subsequence check).

        Extra tools between required steps are allowed. For example, the sequence
        ["search", "calculate"] passes for ["search", "fetch", "calculate"] (fetch
        is permitted in between). Use ``expected_tools_exact=True`` with
        ``expected_tools`` if you also want to forbid extras.
        """
        expected_seq: list[str] = self.expected["tool_sequence"]
        actual_seq = trajectory.tool_sequence

        # Greedy subsequence matching.
        expected_iter = iter(expected_seq)
        current = next(expected_iter, None)
        for tool in actual_seq:
            if current is None:
                break
            if tool == current:
                current = next(expected_iter, None)

        passed = current is None  # consumed all expected elements
        if passed:
            msg = f"tool sequence satisfied: {expected_seq}"
        else:
            msg = f"tool sequence not satisfied: expected {expected_seq}, got {actual_seq}"

        return TrajectoryAssertionResult(name="tool_sequence", passed=passed, message=msg)

    def _check_used_tool(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """Each named tool (or all tools in a list) must appear in the trajectory."""
        required = self.expected["used_tool"]
        if isinstance(required, str):
            required = [required]
        missing = [t for t in required if t not in trajectory.tool_set]
        passed = not missing
        msg = f"required tool(s) used: {required}" if passed else f"required tool(s) not used: {missing}"
        return TrajectoryAssertionResult(name="used_tool", passed=passed, message=msg)

    def _check_did_not_use(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """Each named forbidden tool must NOT appear in the trajectory."""
        forbidden = self.expected["did_not_use"]
        if isinstance(forbidden, str):
            forbidden = [forbidden]
        used = [t for t in forbidden if t in trajectory.tool_set]
        passed = not used
        msg = f"forbidden tool(s) not used: {list(forbidden)}" if passed else f"forbidden tool(s) were used: {used}"
        return TrajectoryAssertionResult(name="did_not_use", passed=passed, message=msg)

    def _check_max_turns(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """Step count must not exceed the configured limit."""
        limit: int = self.expected["max_turns"]
        actual = trajectory.step_count
        passed = actual <= limit
        msg = f"{actual} step(s) (max {limit})"
        return TrajectoryAssertionResult(name="max_turns", passed=passed, message=msg)

    def _check_step_count(self, trajectory: Trajectory) -> TrajectoryAssertionResult:
        """Step count must equal the configured value exactly."""
        expected_count: int = self.expected["step_count"]
        actual = trajectory.step_count
        passed = actual == expected_count
        msg = f"step count: {actual} (expected {expected_count})"
        return TrajectoryAssertionResult(name="step_count", passed=passed, message=msg)

    # ── Optional LLM judge (non-deterministic; off by default) ───────────────

    async def _run_judge(self, trajectory: Trajectory) -> ScorerResult:
        """Run an LLM judge over the rendered trajectory trace.

        NON-DETERMINISTIC. Called only when ``judge=True`` was passed at
        construction. Reuses :class:`~jamjet.eval.scorers.LlmJudgeScorer` so
        provider detection (Anthropic / OpenAI / Ollama) is shared.
        """
        from jamjet.eval.scorers import LlmJudgeScorer  # noqa: PLC0415

        judge = LlmJudgeScorer(
            rubric=self._judge_rubric,
            model=self._judge_model,
            model_fn=self._judge_model_fn,
        )
        trace_text = trajectory.render()
        return await judge.score(trace_text, expected=None)
