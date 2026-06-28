"""
EvalRunner — runs a dataset through a JamJet workflow and applies scorers.

Usage::

    from jamjet.eval import EvalDataset, EvalRunner, LlmJudgeScorer, AssertionScorer

    dataset = EvalDataset.from_file("qa_pairs.jsonl")
    runner = EvalRunner(
        workflow_id="my_workflow",
        scorers=[
            LlmJudgeScorer(rubric="Rate completeness and accuracy 1-5", min_score=3),
            AssertionScorer(checks=["'answer' in output"]),
        ],
        runtime="http://localhost:7700",
    )
    results = await runner.run(dataset)
    runner.print_summary(results)
"""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass
from typing import Any

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.scorers import BaseScorer, ScorerResult
from jamjet.eval.trajectory import Trajectory, TrajectoryScorer


@dataclass
class EvalResult:
    row_id: str
    input: dict[str, Any]
    expected: Any | None
    output: Any | None
    scorers: list[ScorerResult]
    duration_ms: float
    cost_usd: float | None
    error: str | None = None
    # Ordered tool names the run actually called (captured for inspection and
    # replay-regression diffing). None when no trajectory could be reconstructed.
    trajectory: list[str] | None = None

    @property
    def passed(self) -> bool:
        # A case passes only when it has no error AND every scorer passed. The
        # TrajectoryScorer (when a case carries an expected_trajectory) is one of
        # those scorers, so a case passes only if output AND trajectory pass.
        return self.error is None and all(s.passed for s in self.scorers)

    @property
    def overall_score(self) -> float | None:
        numeric = [s.score for s in self.scorers if s.score is not None]
        if not numeric:
            return None
        return sum(numeric) / len(numeric)


async def _score_trajectory(
    row: EvalRow,
    trajectory: Trajectory,
    output: Any,
    scorer_results: list[ScorerResult],
) -> None:
    """Score the run's trajectory IN ADDITION to the output scorers.

    When ``row.expected_trajectory`` is set, a TrajectoryScorer scores the
    reconstructed trajectory and its result is appended to ``scorer_results``
    (alongside, not replacing, the output scorers). A row with no
    expected_trajectory is left output-only (no-op).
    """
    if row.expected_trajectory is None:
        return
    tscorer = TrajectoryScorer(expected=row.expected_trajectory)
    try:
        scorer_results.append(await tscorer.score(output, trajectory=trajectory))
    except Exception as e:
        scorer_results.append(
            ScorerResult(
                scorer=tscorer.name,
                passed=False,
                score=None,
                message=f"scorer error: {e}",
            )
        )


def _trajectory_cell(result: EvalResult) -> str:
    """Render the trajectory scorer's pass/fail (+ failed assertions) for summaries."""
    for s in result.scorers:
        if s.scorer == "trajectory":
            icon = "[green]✓[/green]" if s.passed else "[red]✗[/red]"
            if s.passed:
                return icon
            failed = [a.name for a in getattr(s, "assertions", []) if not a.passed]
            return f"{icon} {', '.join(failed)}" if failed else f"{icon} {s.message}"
    return "—"


def _print_summary_table(results: list[EvalResult], *, console: Any = None) -> None:
    """Print a Rich summary table of eval results.

    The per-case row shows the overall pass/fail (output AND trajectory), the
    output-scorer score, a dedicated Trajectory column (pass/fail + the failed
    trajectory assertions), and the per-scorer detail string.
    """
    from rich.console import Console
    from rich.table import Table

    if console is None:
        console = Console()

    total = len(results)
    passed = sum(1 for r in results if r.passed)
    failed = total - passed
    pass_rate = passed / total * 100 if total else 0

    console.rule(f"[bold]Eval Results — {passed}/{total} passed ({pass_rate:.1f}%)[/bold]")

    table = Table(show_header=True, header_style="bold")
    table.add_column("Row", style="dim")
    table.add_column("Passed")
    table.add_column("Score")
    table.add_column("Trajectory")
    table.add_column("Duration (ms)", justify="right")
    table.add_column("Cost (USD)", justify="right")
    table.add_column("Scorer Details")

    for r in results:
        passed_icon = "[green]✓[/green]" if r.passed else "[red]✗[/red]"
        score_str = f"{r.overall_score:.2f}" if r.overall_score is not None else "—"
        cost_str = f"${r.cost_usd:.6f}" if r.cost_usd is not None else "—"
        details = "; ".join(f"{s.scorer}={'✓' if s.passed else '✗'}({s.message})" for s in r.scorers)
        if r.error:
            details = f"[red]{r.error}[/red]"

        table.add_row(
            r.row_id,
            passed_icon,
            score_str,
            _trajectory_cell(r),
            f"{r.duration_ms:.0f}",
            cost_str,
            details,
        )

    console.print(table)
    console.print(
        f"[bold]Pass rate:[/bold] {pass_rate:.1f}%  [bold]Passed:[/bold] {passed}  [bold]Failed:[/bold] {failed}"
    )


class EvalRunner:
    """Runs an eval dataset against a JamJet workflow via the runtime API."""

    def __init__(
        self,
        workflow_id: str,
        scorers: list[BaseScorer],
        *,
        runtime: str = "http://localhost:7700",
        concurrency: int = 4,
        poll_interval_s: float = 1.0,
        timeout_s: float = 120.0,
    ) -> None:
        self.workflow_id = workflow_id
        self.scorers = scorers
        self.runtime = runtime
        self.concurrency = concurrency
        self.poll_interval_s = poll_interval_s
        self.timeout_s = timeout_s

    async def run(self, dataset: EvalDataset) -> list[EvalResult]:
        """Run all dataset rows, applying scorers to each output."""
        semaphore = asyncio.Semaphore(self.concurrency)

        async def _run_row(row: EvalRow) -> EvalResult:
            async with semaphore:
                return await self._run_one(row)

        tasks = [_run_row(row) for row in dataset]
        return await asyncio.gather(*tasks)

    async def _run_one(self, row: EvalRow) -> EvalResult:
        from jamjet.client import JamjetClient

        start = time.monotonic()
        output = None
        cost_usd = None
        error = None
        events_list: list[dict[str, Any]] | None = None

        try:
            async with JamjetClient(base_url=self.runtime) as client:
                resp = await client.start_execution(
                    workflow_id=self.workflow_id,
                    input=row.input,
                )
                exec_id = resp.get("execution_id", "")

                # Poll until terminal.
                deadline = time.monotonic() + self.timeout_s
                terminal = {"completed", "failed", "cancelled", "limit_exceeded"}
                state: dict = {}
                while time.monotonic() < deadline:
                    await asyncio.sleep(self.poll_interval_s)
                    state = await client.get_execution(exec_id)
                    if state.get("status") in terminal:
                        break
                else:
                    error = f"timeout after {self.timeout_s}s"

                if not error:
                    status = state.get("status")
                    if status == "completed":
                        output = state.get("output") or state.get("state", {})
                    else:
                        error = f"execution {status}: {state.get('error', '')}"

                    # Fetch the event log once: reused for both cost extraction
                    # and trajectory reconstruction (the trajectory-eval hook).
                    try:
                        events_data = await client.get_events(exec_id)
                        events_list = events_data.get("events", [])
                        for evt in events_list:
                            kind = evt.get("kind", {})
                            if kind.get("type") == "node_completed":
                                cost = kind.get("cost_usd")
                                if cost is not None:
                                    cost_usd = (cost_usd or 0.0) + cost
                    except Exception:
                        pass

        except Exception as e:
            error = str(e)

        duration_ms = (time.monotonic() - start) * 1000.0

        # Run scorers against the output.
        scorer_results: list[ScorerResult] = []
        if output is not None:
            for scorer in self.scorers:
                try:
                    result = await scorer.score(
                        output,
                        expected=row.expected,
                        duration_ms=duration_ms,
                        cost_usd=cost_usd,
                        input_data=row.input,
                    )
                    scorer_results.append(result)
                except Exception as e:
                    scorer_results.append(
                        ScorerResult(
                            scorer=scorer.name,
                            passed=False,
                            score=None,
                            message=f"scorer error: {e}",
                        )
                    )

        # Trajectory scoring (T5-4): reconstruct the run's trajectory from the
        # event log and, when the row carries an expected_trajectory, score it
        # IN ADDITION to the output scorers. tool_sequence is captured regardless
        # for inspection / replay-regression diffing.
        trajectory_seq: list[str] | None = None
        if events_list is not None:
            traj = Trajectory.from_events(events_list)
            trajectory_seq = traj.tool_sequence
            await _score_trajectory(row, traj, output, scorer_results)

        return EvalResult(
            row_id=row.id,
            input=row.input,
            expected=row.expected,
            output=output,
            scorers=scorer_results,
            duration_ms=duration_ms,
            cost_usd=cost_usd,
            error=error,
            trajectory=trajectory_seq,
        )

    @staticmethod
    def print_summary(results: list[EvalResult], *, console: Any = None) -> None:
        """Print a Rich summary table of eval results."""
        _print_summary_table(results, console=console)


class AgentEvalRunner:
    """Runs an eval dataset using in-process Agent execution. No runtime server needed.

    Each eval row's input is expected to contain ``_condition`` metadata with
    ``strategy`` and ``model`` keys. A fresh Agent is built per row, so each
    row can test a different strategy/model combination.

    Usage::

        runner = AgentEvalRunner(
            scorers=[LlmJudgeScorer(rubric="...", min_score=3)],
            instructions="You are a helpful assistant.",
            tools=[my_tool],
            concurrency=2,
        )
        results = await runner.run(dataset)
    """

    def __init__(
        self,
        scorers: list[BaseScorer],
        *,
        instructions: str = "",
        tools: list[Any] | None = None,
        concurrency: int = 4,
        timeout_s: float = 120.0,
    ) -> None:
        self.scorers = scorers
        self.instructions = instructions
        self.tools = tools or []
        self.concurrency = concurrency
        self.timeout_s = timeout_s

    async def run(self, dataset: EvalDataset) -> list[EvalResult]:
        """Run all dataset rows, applying scorers to each output."""
        semaphore = asyncio.Semaphore(self.concurrency)

        async def _run_row(row: EvalRow) -> EvalResult:
            async with semaphore:
                return await self._run_one(row)

        tasks = [_run_row(row) for row in dataset]
        return await asyncio.gather(*tasks)

    async def _run_one(self, row: EvalRow) -> EvalResult:
        from jamjet.agents.agent import Agent

        start = time.monotonic()
        output = None
        cost_usd = None
        error = None
        agent_result_obj: Any = None

        condition = row.input.get("_condition", {})
        strategy = condition.get("strategy", "plan-and-execute")
        model = condition.get("model", "")
        task_text = row.input.get("task", str(row.input))

        try:
            agent = Agent(
                name="eval-agent",
                model=model,
                tools=self.tools,
                instructions=self.instructions,
                strategy=strategy,
                max_iterations=10,
                max_cost_usd=1.0,
                timeout_seconds=int(self.timeout_s),
            )
            agent_result = await asyncio.wait_for(
                agent.run(task_text),
                timeout=self.timeout_s,
            )
            agent_result_obj = agent_result
            output = agent_result.output
        except TimeoutError:
            error = f"timeout after {self.timeout_s}s"
        except Exception as e:
            error = str(e)

        duration_ms = (time.monotonic() - start) * 1000.0

        scorer_results: list[ScorerResult] = []
        if output is not None:
            for s in self.scorers:
                try:
                    sr = await s.score(
                        output,
                        expected=row.expected,
                        duration_ms=duration_ms,
                        cost_usd=cost_usd,
                        input_data=row.input,
                    )
                    scorer_results.append(sr)
                except Exception as e:
                    scorer_results.append(
                        ScorerResult(
                            scorer=s.name,
                            passed=False,
                            score=None,
                            message=f"scorer error: {e}",
                        )
                    )

        # Trajectory scoring (T5-4): build the trajectory from the in-process
        # AgentResult.tool_calls and score it IN ADDITION to the output scorers
        # when the row carries an expected_trajectory.
        trajectory_seq: list[str] | None = None
        if agent_result_obj is not None:
            traj = Trajectory.from_agent_result(agent_result_obj)
            trajectory_seq = traj.tool_sequence
            await _score_trajectory(row, traj, output, scorer_results)

        return EvalResult(
            row_id=row.id,
            input=row.input,
            expected=row.expected,
            output=output,
            scorers=scorer_results,
            duration_ms=duration_ms,
            cost_usd=cost_usd,
            error=error,
            trajectory=trajectory_seq,
        )

    @staticmethod
    def print_summary(results: list[EvalResult], *, console: Any = None) -> None:
        """Print a Rich summary table of eval results."""
        _print_summary_table(results, console=console)
