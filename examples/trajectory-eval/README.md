# trajectory-eval

A runnable evalset that shows how to add `expected_trajectory` assertions to
`jamjet eval run` cases, and how to use `jamjet eval trajectory-diff` as a
replay-regression gate.

## What trajectory eval does

When an `EvalRow` carries an `expected_trajectory` key, the runner scores the
run's tool sequence (the ordered list of tools the agent called) in addition to
the final output. A row passes only when both the output scorer and every
trajectory assertion pass.

Trajectory assertions are **deterministic** -- same event log, same spec, same
result, always. No model calls are made unless you opt in with `judge=True`.

## Dataset (`evalset.jsonl`)

```text
search-then-calc   expects ["web_search", "calculator"] in order, max 4 model turns
search-only        expects web_search was used; calculator was NOT used
no-trajectory      no expected_trajectory -- output scoring only (backward compatible)
```

## Run the eval

```bash
# Score outputs with an assertion + check trajectories
jamjet eval run evalset.jsonl \
  --workflow my-research-agent \
  --assert "'answer' in output" \
  --output results.json

# CI: fail if pass rate drops below 80%
jamjet eval run evalset.jsonl \
  --workflow my-research-agent \
  --fail-below 80 \
  --output results.json
```

The summary table includes a `Trajectory` column showing pass/fail for each
trajectory assertion alongside the output scorer.

## Replay-regression diff

After a model or prompt change, diff two runs' tool sequences to catch unexpected
behavioral shifts:

```bash
# Compare two event-log files (before and after a model change)
jamjet eval trajectory-diff before_events.json after_events.json

# Compare results files from two eval runs, scoped to one row
jamjet eval trajectory-diff v1_results.json v2_results.json --row-id search-then-calc

# CI gate: exit 1 if the trajectory changed
jamjet eval trajectory-diff v1.json v2.json --fail-on-change

# Just report, do not fail
jamjet eval trajectory-diff a.json b.json --no-fail-on-change

# Machine-readable output
jamjet eval trajectory-diff a.json b.json --format json
```

## Python API

```python
import asyncio
from pathlib import Path
from jamjet.eval.dataset import EvalDataset
from jamjet.eval.runner import EvalRunner
from jamjet.eval.scorers import AssertionScorer

ds = EvalDataset.from_file(Path(__file__).parent / "evalset.jsonl")

runner = EvalRunner(
    workflow_id="my-research-agent",
    scorers=[AssertionScorer(checks=["'answer' in output"])],
)
results = asyncio.run(runner.run(ds))
EvalRunner.print_summary(results)
```

## expected_trajectory spec keys

| Key | Type | Assertion |
|-----|------|-----------|
| `tool_sequence` | `list[str]` | Tools appear in this order (ordered subsequence; extras allowed in between) |
| `expected_tools` | `list[str]` | All these tools appear (subset; add `"expected_tools_exact": true` to forbid extras) |
| `used_tool` | `str` or `list[str]` | Each named tool appears at least once |
| `did_not_use` | `str` or `list[str]` | None of these tools appear |
| `max_turns` | `int` | Model turn count is at most this value (a turn may emit several parallel tool calls) |
| `step_count` | `int` | Total step count equals this value exactly |

All keys are optional. Only the keys that are present in `expected_trajectory` are checked.
