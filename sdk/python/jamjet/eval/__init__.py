"""
JamJet Eval — evaluation harness for workflows and agents (3.12–3.17).

Provides:
- `EvalDataset`  — loads JSONL/JSON dataset files
- `EvalRunner`   — runs scorers against model outputs
- `ExperimentGrid` / `GridResults` — run experiments across conditions and seeds
- Scorer base classes and built-in scorers
- Custom scorer plugin registry (``ScorerRegistry``, ``@scorer`` decorator)
- `jamjet eval run --dataset` CLI command
- `jamjet eval export` — export results as LaTeX, CSV, or JSON
"""

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.grid import ComparisonResult, ExperimentGrid, GridResults
from jamjet.eval.registry import (
    ScorerDefinition,
    ScorerRegistry,
    get_scorer_registry,
    invoke_scorer,
    scorer,
)
from jamjet.eval.registry import (
    ScorerResult as CustomScorerResult,
)
from jamjet.eval.runner import AgentEvalRunner, EvalResult, EvalRunner
from jamjet.eval.scorers import (
    AssertionScorer,
    BaseScorer,
    CostScorer,
    LatencyScorer,
    LlmJudgeScorer,
    ScorerResult,
)

__all__ = [
    "AgentEvalRunner",
    "AssertionScorer",
    "BaseScorer",
    "ComparisonResult",
    "CostScorer",
    "CustomScorerResult",
    "EvalDataset",
    "EvalResult",
    "EvalRow",
    "EvalRunner",
    "ExperimentGrid",
    "GridResults",
    "LatencyScorer",
    "LlmJudgeScorer",
    "ScorerDefinition",
    "ScorerRegistry",
    "ScorerResult",
    "get_scorer_registry",
    "invoke_scorer",
    "scorer",
]
