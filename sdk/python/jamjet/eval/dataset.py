"""
EvalDataset — loads evaluation datasets from JSONL or JSON files.

Dataset format (JSONL — one JSON object per line):
    {"input": {"query": "..."}, "expected": "...", "metadata": {...}}

Or JSON array:
    [{"input": {...}, "expected": "..."}, ...]

Fields:
- `input` (required)  — workflow/agent input dict
- `expected` (optional) — expected output (for assertion scorers)
- `metadata` (optional) — arbitrary tags, passed through to results
- `id` (optional) — row identifier; auto-assigned if absent
- `expected_trajectory` (optional) — the TrajectoryScorer spec for this case
  (e.g. ``{"tool_sequence": ["search", "calc"]}``). When present, the runner
  scores the run's trajectory IN ADDITION to the final output.

Datasets load from `.json` (top-level array), `.yaml`/`.yml` (top-level list),
or JSONL (one object per line).
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass
class EvalRow:
    id: str
    input: dict[str, Any]
    expected: Any | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
    # Optional TrajectoryScorer spec (tool_sequence / expected_tools / used_tool /
    # did_not_use / max_turns / step_count). None -> output-only scoring.
    expected_trajectory: dict[str, Any] | None = None


class EvalDataset:
    """Loads and iterates over an eval dataset."""

    def __init__(self, rows: list[EvalRow]) -> None:
        self._rows = rows

    @classmethod
    def from_file(cls, path: str | Path) -> EvalDataset:
        """Load a dataset from a JSON array, a YAML list, or a JSONL file."""
        path = Path(path)
        if not path.exists():
            raise FileNotFoundError(f"Dataset file not found: {path}")

        text = path.read_text()
        rows: list[EvalRow] = []

        if path.suffix == ".json":
            data = json.loads(text)
            if not isinstance(data, list):
                raise ValueError("JSON dataset must be a top-level array")
            raw_rows = data
        elif path.suffix in (".yaml", ".yml"):
            import yaml

            data = yaml.safe_load(text)
            if not isinstance(data, list):
                raise ValueError("YAML dataset must be a top-level list")
            raw_rows = data
        else:
            # JSONL: one JSON object per line, skip blanks and comments
            raw_rows = [
                json.loads(line) for line in text.splitlines() if line.strip() and not line.strip().startswith("#")
            ]

        for i, raw in enumerate(raw_rows):
            if "input" not in raw:
                raise ValueError(f"Row {i}: missing required 'input' field")
            rows.append(
                EvalRow(
                    id=str(raw.get("id", f"row_{i}")),
                    input=raw["input"],
                    expected=raw.get("expected"),
                    metadata=raw.get("metadata", {}),
                    expected_trajectory=raw.get("expected_trajectory"),
                )
            )

        return cls(rows)

    def __len__(self) -> int:
        return len(self._rows)

    def __iter__(self) -> Iterator[EvalRow]:
        return iter(self._rows)

    def __getitem__(self, idx: int) -> EvalRow:
        return self._rows[idx]
