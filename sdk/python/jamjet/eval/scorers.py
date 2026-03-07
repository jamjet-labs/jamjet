"""
Built-in eval scorers for JamJet.

Each scorer receives the model output and optionally the expected value,
and returns a `ScorerResult` indicating pass/fail, a numeric score, and a message.

Custom scorers should subclass `BaseScorer` and implement `score()`.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any


@dataclass
class ScorerResult:
    scorer: str
    passed: bool
    score: float | None
    message: str


class BaseScorer(ABC):
    """Abstract base class for eval scorers."""

    name: str = "base"

    @abstractmethod
    async def score(
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
    ) -> ScorerResult: ...


class AssertionScorer(BaseScorer):
    """Evaluates Python boolean expressions against the output.

    Each check is a Python expression string. The variable ``output``
    is bound to the model output, and ``expected`` is bound to the
    expected value from the dataset row.

    Example checks::

        checks=["'sources' in output", "len(output['sources']) >= 3"]
    """

    name = "assertion"

    def __init__(self, checks: list[str]) -> None:
        self.checks = checks

    async def score(
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
    ) -> ScorerResult:
        failures = []
        for check in self.checks:
            try:
                result = eval(check, {"output": output, "expected": expected})  # noqa: S307
                if not result:
                    failures.append(check)
            except Exception as e:
                failures.append(f"{check} (error: {e})")

        passed = len(failures) == 0
        return ScorerResult(
            scorer=self.name,
            passed=passed,
            score=1.0 if passed else 0.0,
            message=(f"all {len(self.checks)} assertions passed" if passed else f"failed: {'; '.join(failures)}"),
        )


class LlmJudgeScorer(BaseScorer):
    """LLM-as-judge: sends output to an LLM with a rubric and expects a 1-5 score.

    Requires an ``anthropic`` or ``openai`` API key in the environment,
    or a ``model_fn`` callable that takes a prompt string and returns
    the completion text.
    """

    name = "llm_judge"

    def __init__(
        self,
        rubric: str,
        model: str = "claude-haiku-4-5-20251001",
        min_score: int = 3,
        model_fn: Any | None = None,
    ) -> None:
        self.rubric = rubric
        self.model = model
        self.min_score = min_score
        self._model_fn = model_fn

    async def score(
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
    ) -> ScorerResult:
        import json

        prompt = (
            f"You are an impartial evaluator.\n\n"
            f"Rubric: {self.rubric}\n\n"
            f"Output to evaluate:\n{json.dumps(output, default=str)}\n\n"
            f'Respond with ONLY a JSON object: {{"score": <integer 1-5>, "reason": "<brief reason>"}}'
        )

        try:
            response_text = await self._call_model(prompt)
            # Extract the JSON block from the response.
            start = response_text.find("{")
            end = response_text.rfind("}") + 1
            if start == -1 or end == 0:
                raise ValueError("No JSON found in judge response")
            parsed = json.loads(response_text[start:end])
            raw_score = int(parsed.get("score", 0))
            reason = parsed.get("reason", "no reason")
            passed = raw_score >= self.min_score
            return ScorerResult(
                scorer=self.name,
                passed=passed,
                score=float(raw_score),
                message=f"score={raw_score}/5 (min={self.min_score}): {reason}",
            )
        except Exception as e:
            return ScorerResult(
                scorer=self.name,
                passed=False,
                score=None,
                message=f"judge failed: {e}",
            )

    async def _call_model(self, prompt: str) -> str:
        if self._model_fn is not None:
            return await self._model_fn(prompt)

        # Auto-detect available SDK.
        try:
            import anthropic

            client = anthropic.Anthropic()
            msg = client.messages.create(
                model=self.model,
                max_tokens=256,
                messages=[{"role": "user", "content": prompt}],
            )
            return msg.content[0].text
        except ImportError:
            pass

        try:
            from openai import OpenAI

            client = OpenAI()
            resp = client.chat.completions.create(
                model=self.model,
                messages=[{"role": "user", "content": prompt}],
                max_tokens=256,
            )
            return resp.choices[0].message.content or ""
        except ImportError:
            pass

        raise RuntimeError(
            "No model SDK available. Install 'anthropic' or 'openai', or pass a custom model_fn to LlmJudgeScorer."
        )


class LatencyScorer(BaseScorer):
    """Passes if execution duration is within the threshold."""

    name = "latency"

    def __init__(self, threshold_ms: float) -> None:
        self.threshold_ms = threshold_ms

    async def score(
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
    ) -> ScorerResult:
        if duration_ms is None:
            return ScorerResult(
                scorer=self.name,
                passed=True,
                score=None,
                message="latency not measured (no duration_ms)",
            )
        passed = duration_ms <= self.threshold_ms
        return ScorerResult(
            scorer=self.name,
            passed=passed,
            score=duration_ms,
            message=f"{duration_ms:.0f}ms (threshold: {self.threshold_ms:.0f}ms)",
        )


class CostScorer(BaseScorer):
    """Passes if estimated cost is within the USD threshold."""

    name = "cost"

    def __init__(self, threshold_usd: float) -> None:
        self.threshold_usd = threshold_usd

    async def score(
        self,
        output: Any,
        *,
        expected: Any | None = None,
        duration_ms: float | None = None,
        cost_usd: float | None = None,
        input_data: Any | None = None,
    ) -> ScorerResult:
        if cost_usd is None:
            return ScorerResult(
                scorer=self.name,
                passed=True,
                score=None,
                message="cost not measured (no cost_usd)",
            )
        passed = cost_usd <= self.threshold_usd
        return ScorerResult(
            scorer=self.name,
            passed=passed,
            score=cost_usd,
            message=f"${cost_usd:.6f} (threshold: ${self.threshold_usd:.4f})",
        )
