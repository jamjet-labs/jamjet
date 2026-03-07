"""
JamJet Eval — evaluation harness for workflows and agents (3.12–3.17).

Provides:
- `EvalDataset`  — loads JSONL/JSON dataset files
- `EvalRunner`   — runs scorers against model outputs
- Scorer base classes and built-in scorers
- `jamjet eval run --dataset` CLI command
"""

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.runner import EvalResult, EvalRunner
from jamjet.eval.scorers import (
    AssertionScorer,
    BaseScorer,
    CostScorer,
    LatencyScorer,
    LlmJudgeScorer,
    ScorerResult,
)

__all__ = [
    "AssertionScorer",
    "BaseScorer",
    "CostScorer",
    "EvalDataset",
    "EvalResult",
    "EvalRow",
    "EvalRunner",
    "LatencyScorer",
    "LlmJudgeScorer",
    "ScorerResult",
]
