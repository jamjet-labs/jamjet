"""
ExperimentGrid — run evaluations across a cartesian product of conditions and seeds.

Usage::

    from jamjet.eval.grid import ExperimentGrid, GridResults
    from jamjet.eval.scorers import LlmJudgeScorer, LatencyScorer

    grid = ExperimentGrid(
        workflow_id="my-workflow",
        conditions={
            "strategy": ["react", "plan-and-execute"],
            "model": ["claude-haiku-4-5-20251001", "gemini-2.0-flash"],
        },
        dataset="evals/dataset.jsonl",
        scorers=[
            LlmJudgeScorer(rubric="Rate quality 1-5", min_score=3),
            LatencyScorer(threshold_ms=5000),
        ],
        seeds=[42, 123, 456],
        runtime="http://localhost:7700",
        concurrency=4,
    )

    results = await grid.run()
    results.summary()
    results.to_csv("results/grid.csv")
    results.to_latex("results/table.tex")
    results.to_json("results/grid.json")
"""

from __future__ import annotations

import asyncio
import csv
import io
import itertools
import json
import math
import warnings
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from jamjet.eval.dataset import EvalDataset
from jamjet.eval.runner import EvalResult, EvalRunner
from jamjet.eval.scorers import BaseScorer, ScorerResult


@dataclass
class ComparisonResult:
    """Result of a statistical comparison between two conditions."""

    test_name: str
    statistic: float | None
    p_value: float | None
    effect_size: float | None
    ci_lower: float | None
    ci_upper: float | None
    significant: bool
    sample_sizes: tuple[int, int]
    mean_a: float = 0.0
    mean_b: float = 0.0
    mean_diff: float = 0.0
    condition_a: str = ""
    condition_b: str = ""


@dataclass
class ConditionResult:
    """Results for a single condition + seed combination."""

    condition: dict[str, str]
    seed: int | None
    eval_results: list[EvalResult]


class GridResults:
    """Aggregated results from an experiment grid run."""

    def __init__(self, results: list[ConditionResult]) -> None:
        self.results = results

    # ── summary ──────────────────────────────────────────────────────────

    def summary(self, *, console: Any = None) -> None:
        """Print a Rich table: rows = conditions, columns = avg score, pass rate, avg latency, avg cost."""
        from rich.console import Console
        from rich.table import Table

        if console is None:
            console = Console()

        table = Table(
            title="Experiment Grid Results",
            show_header=True,
            header_style="bold",
        )
        table.add_column("Condition", style="cyan")
        table.add_column("Seed", style="dim")
        table.add_column("Rows", justify="right")
        table.add_column("Pass Rate", justify="right")
        table.add_column("Avg Score", justify="right")
        table.add_column("Avg Latency (ms)", justify="right")
        table.add_column("Avg Cost (USD)", justify="right")

        for cr in self.results:
            cond_str = ", ".join(f"{k}={v}" for k, v in sorted(cr.condition.items()))
            seed_str = str(cr.seed) if cr.seed is not None else "-"
            n = len(cr.eval_results)
            passed = sum(1 for r in cr.eval_results if r.passed)
            pass_rate = f"{passed / n * 100:.1f}%" if n else "-"

            scores = [r.overall_score for r in cr.eval_results if r.overall_score is not None]
            avg_score = f"{sum(scores) / len(scores):.2f}" if scores else "-"

            latencies = [r.duration_ms for r in cr.eval_results]
            avg_lat = f"{sum(latencies) / len(latencies):.0f}" if latencies else "-"

            costs = [r.cost_usd for r in cr.eval_results if r.cost_usd is not None]
            avg_cost = f"${sum(costs) / len(costs):.6f}" if costs else "-"

            table.add_row(cond_str, seed_str, str(n), pass_rate, avg_score, avg_lat, avg_cost)

        console.print(table)

        # Print aggregated stats grouped by condition (across seeds).
        agg = self._aggregate_by_condition()
        if len(agg) > 1 or (len(agg) == 1 and any(cr.seed is not None for cr in self.results)):
            console.print()
            agg_table = Table(
                title="Aggregated by Condition (across seeds)",
                show_header=True,
                header_style="bold",
            )
            agg_table.add_column("Condition", style="cyan")
            agg_table.add_column("Seeds", justify="right")
            agg_table.add_column("Total Rows", justify="right")
            agg_table.add_column("Pass Rate", justify="right")
            agg_table.add_column("Avg Score", justify="right")

            for cond_key, evals in agg.items():
                n = len(evals)
                passed = sum(1 for r in evals if r.passed)
                pass_rate = f"{passed / n * 100:.1f}%" if n else "-"
                scores = [r.overall_score for r in evals if r.overall_score is not None]
                avg_score = f"{sum(scores) / len(scores):.2f}" if scores else "-"
                seed_count = sum(1 for cr in self.results if self._condition_key(cr.condition) == cond_key)
                agg_table.add_row(cond_key, str(seed_count), str(n), pass_rate, avg_score)

            console.print(agg_table)

    # ── export: CSV ──────────────────────────────────────────────────────

    def to_csv(self, path: str) -> None:
        """Export results to a CSV file."""
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w", newline="") as f:
            self._write_csv(f)

    def format_csv(self) -> str:
        """Return results as a CSV string."""
        buf = io.StringIO()
        self._write_csv(buf)
        return buf.getvalue()

    def _write_csv(self, f: Any) -> None:
        writer = csv.writer(f)
        # Collect all condition keys across all results.
        cond_keys = self._all_condition_keys()
        cond_cols = [f"condition_{k}" for k in cond_keys]
        header = [*cond_cols, "seed", "row_id", "passed", "score", "duration_ms", "cost_usd"]
        writer.writerow(header)
        for cr in self.results:
            for r in cr.eval_results:
                row = [
                    *[cr.condition.get(k, "") for k in cond_keys],
                    cr.seed if cr.seed is not None else "",
                    r.row_id,
                    r.passed,
                    r.overall_score if r.overall_score is not None else "",
                    f"{r.duration_ms:.1f}",
                    f"{r.cost_usd:.6f}" if r.cost_usd is not None else "",
                ]
                writer.writerow(row)

    # ── export: LaTeX ────────────────────────────────────────────────────

    def to_latex(self, path: str, *, caption: str = "Experiment Results") -> None:
        """Export results to a LaTeX table file."""
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w") as f:
            f.write(self.format_latex(caption=caption))

    def format_latex(self, *, caption: str = "Experiment Results") -> str:
        """Return results as a LaTeX table string (booktabs style, mean +/- std)."""
        agg = self._aggregate_by_condition()
        cond_keys = self._all_condition_keys()

        # Build column spec: one column per condition key + 4 metric columns.
        n_cond = len(cond_keys)
        col_spec = "l" * n_cond + "cccc"

        lines: list[str] = []
        lines.append(r"\begin{table}[htbp]")
        lines.append(r"\centering")
        lines.append(rf"\caption{{{_latex_escape(caption)}}}")
        lines.append(rf"\begin{{tabular}}{{{col_spec}}}")
        lines.append(r"\toprule")

        # Header row.
        cond_hdrs = [_latex_escape(k.replace("_", " ").title()) for k in cond_keys]
        headers = [*cond_hdrs, "Pass Rate", "Score", "Latency (ms)", "Cost (USD)"]
        lines.append(" & ".join(headers) + r" \\")
        lines.append(r"\midrule")

        for cond_key, evals in agg.items():
            # Parse condition key back to values.
            cond_dict = dict(part.split("=", 1) for part in cond_key.split(", "))

            n = len(evals)
            passed = sum(1 for r in evals if r.passed)
            pass_rate = f"{passed / n * 100:.1f}\\%" if n else "--"

            scores = [r.overall_score for r in evals if r.overall_score is not None]
            score_str = _mean_std_str(scores) if scores else "--"

            latencies = [r.duration_ms for r in evals]
            lat_str = _mean_std_str(latencies, fmt=".0f") if latencies else "--"

            costs = [r.cost_usd for r in evals if r.cost_usd is not None]
            cost_str = _mean_std_str(costs, fmt=".4f") if costs else "--"

            cond_vals = [_latex_escape(cond_dict.get(k, "")) for k in cond_keys]
            row_cells = [*cond_vals, pass_rate, score_str, lat_str, cost_str]
            lines.append(" & ".join(row_cells) + r" \\")

        lines.append(r"\bottomrule")
        lines.append(r"\end{tabular}")
        lines.append(r"\label{tab:experiment-results}")
        lines.append(r"\end{table}")
        return "\n".join(lines) + "\n"

    # ── export: JSON ─────────────────────────────────────────────────────

    def to_json(self, path: str) -> None:
        """Export full results to a JSON file."""
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        with open(path, "w") as f:
            json.dump(self._to_serializable(), f, indent=2, default=str)

    def format_json(self) -> str:
        """Return results as a JSON string."""
        return json.dumps(self._to_serializable(), indent=2, default=str)

    def _to_serializable(self) -> list[dict]:
        out = []
        for cr in self.results:
            out.append(
                {
                    "condition": cr.condition,
                    "seed": cr.seed,
                    "eval_results": [
                        {
                            "row_id": r.row_id,
                            "passed": r.passed,
                            "overall_score": r.overall_score,
                            "duration_ms": r.duration_ms,
                            "cost_usd": r.cost_usd,
                            "error": r.error,
                            "scorers": [asdict(s) for s in r.scorers],
                            "input": r.input,
                            "expected": r.expected,
                            "output": r.output,
                        }
                        for r in cr.eval_results
                    ],
                }
            )
        return out

    # ── compare ──────────────────────────────────────────────────────────

    def compare(
        self,
        condition_a: dict[str, str],
        condition_b: dict[str, str],
        *,
        test: str = "welch",
        alpha: float = 0.05,
    ) -> ComparisonResult:
        """Compare two conditions statistically.

        Args:
            condition_a: First condition dict (e.g. {"strategy": "react"}).
            condition_b: Second condition dict (e.g. {"strategy": "debate"}).
            test: Statistical test to use. One of "welch", "wilcoxon",
                  "mann_whitney", or "auto". Default is "welch".
            alpha: Significance level (default 0.05).

        Returns:
            A ComparisonResult dataclass with test statistics, p-value,
            effect size, confidence interval, and significance flag.

        Raises:
            ValueError: If a condition is not found or data is insufficient.
        """
        key_a = self._condition_key(condition_a)
        key_b = self._condition_key(condition_b)
        agg = self._aggregate_by_condition()

        if key_a not in agg:
            raise ValueError(f"condition not found: {key_a}")
        if key_b not in agg:
            raise ValueError(f"condition not found: {key_b}")

        scores_a = [r.overall_score for r in agg[key_a] if r.overall_score is not None]
        scores_b = [r.overall_score for r in agg[key_b] if r.overall_score is not None]

        if not scores_a or not scores_b:
            raise ValueError("insufficient score data for comparison")

        return _run_comparison(scores_a, scores_b, test=test, alpha=alpha, label_a=key_a, label_b=key_b)

    # ── from_json ────────────────────────────────────────────────────────

    @classmethod
    def from_json(cls, path: str) -> GridResults:
        """Load GridResults from a JSON file previously saved via to_json().

        The JSON is expected to be a list of objects, each with keys:
        ``condition``, ``seed``, and ``eval_results``.
        """
        with open(path) as f:
            data = json.load(f)

        if not isinstance(data, list):
            raise ValueError("expected a JSON array of condition results")

        results: list[ConditionResult] = []
        for entry in data:
            condition = entry.get("condition", {})
            seed = entry.get("seed")
            eval_results: list[EvalResult] = []
            for er in entry.get("eval_results", []):
                scorer_dicts = er.get("scorers", [])
                scorers = [
                    ScorerResult(
                        scorer=s.get("scorer", ""),
                        passed=s.get("passed", False),
                        score=s.get("score"),
                        message=s.get("message", ""),
                    )
                    for s in scorer_dicts
                ]
                eval_results.append(
                    EvalResult(
                        row_id=er.get("row_id", ""),
                        input=er.get("input", {}),
                        expected=er.get("expected"),
                        output=er.get("output"),
                        scorers=scorers,
                        duration_ms=er.get("duration_ms", 0.0),
                        cost_usd=er.get("cost_usd"),
                        error=er.get("error"),
                    )
                )
            results.append(ConditionResult(condition=condition, seed=seed, eval_results=eval_results))

        return cls(results)

    # ── helpers ───────────────────────────────────────────────────────────

    @staticmethod
    def _condition_key(condition: dict[str, str]) -> str:
        return ", ".join(f"{k}={v}" for k, v in sorted(condition.items()))

    def _all_condition_keys(self) -> list[str]:
        """Return sorted list of all condition parameter names."""
        keys: set[str] = set()
        for cr in self.results:
            keys.update(cr.condition.keys())
        return sorted(keys)

    def _aggregate_by_condition(self) -> dict[str, list[EvalResult]]:
        """Group eval results by condition (across seeds)."""
        agg: dict[str, list[EvalResult]] = {}
        for cr in self.results:
            key = self._condition_key(cr.condition)
            if key not in agg:
                agg[key] = []
            agg[key].extend(cr.eval_results)
        return agg


class ExperimentGrid:
    """Run evaluations across a cartesian product of conditions and seeds."""

    def __init__(
        self,
        workflow_id: str,
        conditions: dict[str, list[str]],
        dataset: str | EvalDataset,
        scorers: list[BaseScorer],
        *,
        seeds: list[int] | None = None,
        runtime: str = "http://localhost:7700",
        concurrency: int = 4,
        poll_interval_s: float = 1.0,
        timeout_s: float = 120.0,
        mode: str = "runtime",
        agent_instructions: str = "",
        agent_tools: list | None = None,
    ) -> None:
        self.workflow_id = workflow_id
        self.conditions = conditions
        self.scorers = scorers
        self.seeds = seeds
        self.runtime = runtime
        self.concurrency = concurrency
        self.poll_interval_s = poll_interval_s
        self.timeout_s = timeout_s
        self.mode = mode
        self.agent_instructions = agent_instructions
        self.agent_tools = agent_tools or []

        if isinstance(dataset, str):
            self._dataset = EvalDataset.from_file(dataset)
        else:
            self._dataset = dataset

    def _condition_combinations(self) -> list[dict[str, str]]:
        """Compute cartesian product of all condition values."""
        keys = sorted(self.conditions.keys())
        value_lists = [self.conditions[k] for k in keys]
        combos = []
        for values in itertools.product(*value_lists):
            combos.append(dict(zip(keys, values)))
        return combos

    async def run(self) -> GridResults:
        """Run the full experiment grid and return aggregated results."""
        combinations = self._condition_combinations()
        seeds: list[int | None] = list(self.seeds) if self.seeds is not None else [None]

        semaphore = asyncio.Semaphore(self.concurrency)
        all_results: list[ConditionResult] = []

        async def _run_condition(condition: dict[str, str], seed: int | None) -> ConditionResult:
            async with semaphore:
                if self.mode == "agent":
                    from jamjet.eval.runner import AgentEvalRunner

                    runner: EvalRunner | AgentEvalRunner = AgentEvalRunner(
                        scorers=self.scorers,
                        instructions=self.agent_instructions,
                        tools=self.agent_tools,
                        concurrency=1,
                        timeout_s=self.timeout_s,
                    )
                else:
                    runner = EvalRunner(
                        workflow_id=self.workflow_id,
                        scorers=self.scorers,
                        runtime=self.runtime,
                        concurrency=1,
                        poll_interval_s=self.poll_interval_s,
                        timeout_s=self.timeout_s,
                    )

                # Build a modified dataset that injects condition + seed metadata
                # into each row's input.
                modified_dataset = self._inject_metadata(condition, seed)
                eval_results = await runner.run(modified_dataset)
                return ConditionResult(
                    condition=condition,
                    seed=seed,
                    eval_results=list(eval_results),
                )

        tasks = []
        for condition in combinations:
            for seed in seeds:
                tasks.append(_run_condition(condition, seed))

        results = await asyncio.gather(*tasks)
        all_results.extend(results)

        return GridResults(all_results)

    def _inject_metadata(self, condition: dict[str, str], seed: int | None) -> EvalDataset:
        """Create a copy of the dataset with condition and seed metadata injected into inputs."""
        from jamjet.eval.dataset import EvalRow

        modified_rows = []
        for row in self._dataset:
            new_input = dict(row.input)
            new_input["_condition"] = condition
            if seed is not None:
                new_input["_seed"] = seed
            modified_rows.append(
                EvalRow(
                    id=row.id,
                    input=new_input,
                    expected=row.expected,
                    metadata=row.metadata,
                )
            )
        return EvalDataset(modified_rows)


# ── Statistical comparison helpers ────────────────────────────────────────────


def _run_comparison(
    scores_a: list[float],
    scores_b: list[float],
    *,
    test: str = "welch",
    alpha: float = 0.05,
    label_a: str = "A",
    label_b: str = "B",
) -> ComparisonResult:
    """Run a statistical comparison between two score lists.

    Supported tests: ``welch``, ``wilcoxon``, ``mann_whitney``, ``auto``.
    Falls back to basic mean-diff (no p-value) if scipy is not installed.
    """
    n_a, n_b = len(scores_a), len(scores_b)
    mean_a = sum(scores_a) / n_a
    mean_b = sum(scores_b) / n_b
    mean_diff = mean_a - mean_b

    # Resolve "auto" test selection.
    resolved_test = _resolve_auto_test(test, scores_a, scores_b)

    try:
        import importlib.util

        if importlib.util.find_spec("scipy") is None:
            raise ImportError("scipy not installed")

        return _scipy_compare(
            scores_a,
            scores_b,
            test=resolved_test,
            alpha=alpha,
            label_a=label_a,
            label_b=label_b,
        )
    except ImportError:
        warnings.warn(
            "scipy is not installed — returning basic mean difference only. "
            "Install scipy for full statistical tests: pip install scipy",
            stacklevel=2,
        )
        # Fallback: just mean diff, no p-value / CI / effect size.
        return ComparisonResult(
            test_name="basic_mean_diff",
            statistic=None,
            p_value=None,
            effect_size=None,
            ci_lower=None,
            ci_upper=None,
            significant=False,
            sample_sizes=(n_a, n_b),
            mean_a=mean_a,
            mean_b=mean_b,
            mean_diff=mean_diff,
            condition_a=label_a,
            condition_b=label_b,
        )


def _resolve_auto_test(test: str, scores_a: list[float], scores_b: list[float]) -> str:
    """Resolve 'auto' to a concrete test name based on data characteristics."""
    if test != "auto":
        return test

    n_a, n_b = len(scores_a), len(scores_b)
    paired = n_a == n_b

    # For small paired samples (n < 30), use Wilcoxon (nonparametric, paired).
    # For small unpaired samples, use Mann-Whitney (nonparametric, independent).
    # For larger samples, use Welch's t-test (robust to unequal variances).
    if paired and n_a < 30:
        return "wilcoxon"
    elif not paired or n_a < 30:
        return "mann_whitney"
    else:
        return "welch"


def _scipy_compare(
    scores_a: list[float],
    scores_b: list[float],
    *,
    test: str,
    alpha: float,
    label_a: str,
    label_b: str,
) -> ComparisonResult:
    """Run scipy-backed statistical comparison."""
    from scipy import stats as sp_stats

    n_a, n_b = len(scores_a), len(scores_b)
    mean_a = sum(scores_a) / n_a
    mean_b = sum(scores_b) / n_b
    mean_diff = mean_a - mean_b

    statistic: float
    p_value: float
    test_name: str

    if test == "welch":
        stat_result = sp_stats.ttest_ind(scores_a, scores_b, equal_var=False)
        statistic = float(stat_result.statistic)
        p_value = float(stat_result.pvalue)
        test_name = "welch_t_test"
    elif test == "wilcoxon":
        if n_a != n_b:
            raise ValueError("wilcoxon test requires paired (equal-length) samples")
        diffs = [a - b for a, b in zip(scores_a, scores_b)]
        if all(d == 0.0 for d in diffs):
            # All differences are zero — no variation to test.
            return ComparisonResult(
                test_name="wilcoxon_signed_rank",
                statistic=0.0,
                p_value=1.0,
                effect_size=0.0,
                ci_lower=0.0,
                ci_upper=0.0,
                significant=False,
                sample_sizes=(n_a, n_b),
                mean_a=mean_a,
                mean_b=mean_b,
                mean_diff=mean_diff,
                condition_a=label_a,
                condition_b=label_b,
            )
        stat_result = sp_stats.wilcoxon(scores_a, scores_b)
        statistic = float(stat_result.statistic)
        p_value = float(stat_result.pvalue)
        test_name = "wilcoxon_signed_rank"
    elif test == "mann_whitney":
        stat_result = sp_stats.mannwhitneyu(scores_a, scores_b, alternative="two-sided")
        statistic = float(stat_result.statistic)
        p_value = float(stat_result.pvalue)
        test_name = "mann_whitney_u"
    else:
        raise ValueError(f"unknown test: {test!r}. Use 'welch', 'wilcoxon', 'mann_whitney', or 'auto'.")

    effect_size = _cohens_d(scores_a, scores_b)
    ci_lower, ci_upper = _confidence_interval(scores_a, scores_b, alpha=alpha)
    significant = p_value < alpha

    return ComparisonResult(
        test_name=test_name,
        statistic=statistic,
        p_value=p_value,
        effect_size=effect_size,
        ci_lower=ci_lower,
        ci_upper=ci_upper,
        significant=significant,
        sample_sizes=(n_a, n_b),
        mean_a=mean_a,
        mean_b=mean_b,
        mean_diff=mean_diff,
        condition_a=label_a,
        condition_b=label_b,
    )


def _cohens_d(a: list[float], b: list[float]) -> float:
    """Compute Cohen's d effect size for two independent samples."""
    n_a, n_b = len(a), len(b)
    mean_a = sum(a) / n_a
    mean_b = sum(b) / n_b

    if n_a < 2 and n_b < 2:
        return 0.0

    var_a = sum((x - mean_a) ** 2 for x in a) / max(n_a - 1, 1)
    var_b = sum((x - mean_b) ** 2 for x in b) / max(n_b - 1, 1)

    # Pooled standard deviation.
    pooled_var = ((n_a - 1) * var_a + (n_b - 1) * var_b) / max(n_a + n_b - 2, 1)
    pooled_sd = math.sqrt(pooled_var)
    if pooled_sd == 0.0:
        return 0.0
    return (mean_a - mean_b) / pooled_sd


def _confidence_interval(a: list[float], b: list[float], *, alpha: float = 0.05) -> tuple[float, float]:
    """Compute confidence interval for the difference in means (Welch approximation)."""
    from scipy import stats as sp_stats

    n_a, n_b = len(a), len(b)
    mean_a = sum(a) / n_a
    mean_b = sum(b) / n_b
    mean_diff = mean_a - mean_b

    var_a = sum((x - mean_a) ** 2 for x in a) / max(n_a - 1, 1) if n_a > 1 else 0.0
    var_b = sum((x - mean_b) ** 2 for x in b) / max(n_b - 1, 1) if n_b > 1 else 0.0

    se = math.sqrt(var_a / n_a + var_b / n_b)
    if se == 0.0:
        return (mean_diff, mean_diff)

    # Welch-Satterthwaite degrees of freedom.
    num = (var_a / n_a + var_b / n_b) ** 2
    denom_a = (var_a / n_a) ** 2 / max(n_a - 1, 1) if n_a > 1 else 0.0
    denom_b = (var_b / n_b) ** 2 / max(n_b - 1, 1) if n_b > 1 else 0.0
    denom = denom_a + denom_b
    df = num / denom if denom > 0 else 1.0

    t_crit = sp_stats.t.ppf(1.0 - alpha / 2.0, df)
    margin = t_crit * se
    return (mean_diff - margin, mean_diff + margin)


# ── Helpers ──────────────────────────────────────────────────────────────────


def _mean_std_str(values: list[float], *, fmt: str = ".2f") -> str:
    """Format a list of values as 'mean +/- std'."""
    n = len(values)
    if n == 0:
        return "--"
    mean = sum(values) / n
    if n == 1:
        return f"${mean:{fmt}}$"
    variance = sum((v - mean) ** 2 for v in values) / (n - 1)
    std = math.sqrt(variance)
    return f"${mean:{fmt}} \\pm {std:{fmt}}$"


def _latex_escape(text: str) -> str:
    """Escape special LaTeX characters."""
    replacements = {
        "&": r"\&",
        "%": r"\%",
        "$": r"\$",
        "#": r"\#",
        "_": r"\_",
        "{": r"\{",
        "}": r"\}",
        "~": r"\textasciitilde{}",
        "^": r"\textasciicircum{}",
    }
    for char, replacement in replacements.items():
        text = text.replace(char, replacement)
    return text
