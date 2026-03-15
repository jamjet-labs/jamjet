"""
Custom eval scorer plugin registry for JamJet.

Provides a ``ScorerRegistry`` that allows users to register custom scorer
functions (sync or async) using either the ``@scorer`` decorator or the
imperative ``register()`` API.  A module-level singleton is available via
``get_scorer_registry()``.

Built-in scorers (``llm_judge``, ``assertion``, ``latency``, ``cost``) are
registered automatically when the registry is first created so that custom
and built-in scorers share a single lookup namespace.

Usage::

    from jamjet.eval.registry import scorer, ScorerResult

    @scorer(name="factuality", description="Check factual accuracy")
    async def factuality_scorer(input: dict, output: dict, context: dict) -> ScorerResult:
        # custom scoring logic
        return ScorerResult(score=0.85, passed=True, reason="Factually accurate")
"""

from __future__ import annotations

import asyncio
import inspect
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any


@dataclass
class ScorerResult:
    """Result returned by a custom scorer function.

    Attributes:
        score: Normalised score in the range ``[0.0, 1.0]``.
        passed: Whether the evaluation is considered a pass.
        reason: Optional human-readable explanation.
        metadata: Optional extra data attached to the result.
    """

    score: float
    passed: bool
    reason: str | None = None
    metadata: dict[str, Any] | None = None


@dataclass
class ScorerDefinition:
    """A named scorer entry in the registry.

    Attributes:
        name: Unique identifier used to reference the scorer.
        fn: Callable that accepts ``(input, output, context)`` and returns
            a :class:`ScorerResult`.  May be sync or async.
        description: Optional human-readable description.
        version: Semver-style version string (default ``"1.0"``).
    """

    name: str
    fn: Callable[..., Any]
    description: str | None = None
    version: str = "1.0"


class ScorerRegistry:
    """Thread-safe registry for custom scorer plugins.

    The registry maps scorer *names* to :class:`ScorerDefinition` instances.
    Duplicate registrations raise :class:`ValueError` by default; pass
    ``overwrite=True`` to ``register()`` to replace an existing entry.
    """

    def __init__(self) -> None:
        self._scorers: dict[str, ScorerDefinition] = {}

    # ── public API ────────────────────────────────────────────────────────

    def register(
        self,
        name: str,
        fn: Callable[..., Any],
        *,
        description: str | None = None,
        version: str = "1.0",
        overwrite: bool = False,
    ) -> None:
        """Register a scorer function under *name*.

        Raises :class:`ValueError` if *name* is already registered and
        *overwrite* is ``False``.
        """
        if name in self._scorers and not overwrite:
            raise ValueError(f"Scorer '{name}' is already registered. Pass overwrite=True to replace it.")
        self._scorers[name] = ScorerDefinition(
            name=name,
            fn=fn,
            description=description,
            version=version,
        )

    def get(self, name: str) -> ScorerDefinition | None:
        """Return the scorer definition for *name*, or ``None``."""
        return self._scorers.get(name)

    def list(self) -> list[str]:
        """Return a sorted list of all registered scorer names."""
        return sorted(self._scorers)

    def unregister(self, name: str) -> None:
        """Remove a scorer by name.

        Raises :class:`KeyError` if *name* is not registered.
        """
        if name not in self._scorers:
            raise KeyError(f"Scorer '{name}' is not registered.")
        del self._scorers[name]

    def __contains__(self, name: str) -> bool:
        return name in self._scorers

    def __len__(self) -> int:
        return len(self._scorers)


# ── Built-in scorer stubs ────────────────────────────────────────────────────


async def _builtin_llm_judge(
    input: dict[str, Any],
    output: dict[str, Any],
    context: dict[str, Any],
) -> ScorerResult:
    """Placeholder LLM-as-judge scorer.

    In production this delegates to the Rust runtime which calls the model.
    The stub returns a neutral score to indicate the scorer was invoked.
    """
    return ScorerResult(
        score=0.5,
        passed=True,
        reason="LLM judge placeholder — runtime executes actual scoring",
        metadata={"builtin": True},
    )


async def _builtin_assertion(
    input: dict[str, Any],
    output: dict[str, Any],
    context: dict[str, Any],
) -> ScorerResult:
    """Placeholder assertion scorer.

    Actual assertion evaluation uses the expression engine in
    :class:`~jamjet.eval.scorers.AssertionScorer`.
    """
    expression = context.get("expression", "")
    try:
        result = eval(expression, {"input": input, "output": output})  # noqa: S307
        passed = bool(result)
    except Exception as exc:
        return ScorerResult(
            score=0.0,
            passed=False,
            reason=f"Assertion error: {exc}",
            metadata={"builtin": True, "expression": expression},
        )
    return ScorerResult(
        score=1.0 if passed else 0.0,
        passed=passed,
        reason=f"Assertion {'passed' if passed else 'failed'}: {expression}",
        metadata={"builtin": True, "expression": expression},
    )


async def _builtin_latency(
    input: dict[str, Any],
    output: dict[str, Any],
    context: dict[str, Any],
) -> ScorerResult:
    """Placeholder latency scorer.

    Scores based on execution time vs a threshold (in milliseconds).
    """
    threshold_ms = context.get("threshold_ms", 5000)
    duration_ms = context.get("duration_ms")
    if duration_ms is None:
        return ScorerResult(
            score=1.0,
            passed=True,
            reason="No latency data available",
            metadata={"builtin": True},
        )
    passed = duration_ms <= threshold_ms
    score = max(0.0, min(1.0, 1.0 - (duration_ms - threshold_ms) / threshold_ms)) if threshold_ms > 0 else 1.0
    return ScorerResult(
        score=score,
        passed=passed,
        reason=f"{duration_ms:.0f}ms vs {threshold_ms:.0f}ms threshold",
        metadata={"builtin": True, "duration_ms": duration_ms, "threshold_ms": threshold_ms},
    )


async def _builtin_cost(
    input: dict[str, Any],
    output: dict[str, Any],
    context: dict[str, Any],
) -> ScorerResult:
    """Placeholder cost scorer.

    Scores based on token cost vs a USD budget.
    """
    threshold_usd = context.get("threshold_usd", 1.0)
    cost_usd = context.get("cost_usd")
    if cost_usd is None:
        return ScorerResult(
            score=1.0,
            passed=True,
            reason="No cost data available",
            metadata={"builtin": True},
        )
    passed = cost_usd <= threshold_usd
    score = max(0.0, min(1.0, 1.0 - (cost_usd - threshold_usd) / threshold_usd)) if threshold_usd > 0 else 1.0
    return ScorerResult(
        score=score,
        passed=passed,
        reason=f"${cost_usd:.6f} vs ${threshold_usd:.4f} budget",
        metadata={"builtin": True, "cost_usd": cost_usd, "threshold_usd": threshold_usd},
    )


def _register_builtins(registry: ScorerRegistry) -> None:
    """Register the four built-in scorer stubs."""
    registry.register("llm_judge", _builtin_llm_judge, description="LLM-as-judge scorer (placeholder)")
    registry.register("assertion", _builtin_assertion, description="Assertion expression scorer (placeholder)")
    registry.register("latency", _builtin_latency, description="Latency threshold scorer (placeholder)")
    registry.register("cost", _builtin_cost, description="Cost budget scorer (placeholder)")


# ── Module-level singleton ───────────────────────────────────────────────────

_SCORER_REGISTRY: ScorerRegistry | None = None


def get_scorer_registry() -> ScorerRegistry:
    """Return the module-level scorer registry singleton.

    On first call the four built-in scorers are auto-registered.
    """
    global _SCORER_REGISTRY  # noqa: PLW0603
    if _SCORER_REGISTRY is None:
        _SCORER_REGISTRY = ScorerRegistry()
        _register_builtins(_SCORER_REGISTRY)
    return _SCORER_REGISTRY


# ── @scorer decorator ────────────────────────────────────────────────────────


def scorer(
    name: str,
    *,
    description: str | None = None,
    version: str = "1.0",
    overwrite: bool = False,
) -> Callable[..., Any]:
    """Decorator that registers a function as a named custom scorer.

    The decorated function must accept ``(input, output, context)`` and return
    a :class:`ScorerResult`.  It may be sync or async.

    Example::

        @scorer(name="factuality", description="Check factual accuracy")
        async def factuality_scorer(input, output, context):
            return ScorerResult(score=0.85, passed=True, reason="Factually accurate")
    """

    def _decorator(fn: Callable[..., Any]) -> Callable[..., Any]:
        registry = get_scorer_registry()
        registry.register(
            name,
            fn,
            description=description,
            version=version,
            overwrite=overwrite,
        )
        return fn

    return _decorator


# ── Helper: invoke a registered scorer ────────────────────────────────────────


async def invoke_scorer(
    name: str,
    input: dict[str, Any],
    output: dict[str, Any],
    context: dict[str, Any] | None = None,
) -> ScorerResult:
    """Look up a scorer by name and invoke it.

    Raises :class:`KeyError` if the scorer is not registered.
    Handles both sync and async scorer functions transparently.
    """
    registry = get_scorer_registry()
    definition = registry.get(name)
    if definition is None:
        raise KeyError(f"Scorer '{name}' is not registered.")

    ctx = context or {}
    if inspect.iscoroutinefunction(definition.fn):
        return await definition.fn(input, output, ctx)
    else:
        # Wrap sync functions so callers always get a coroutine.
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, definition.fn, input, output, ctx)
