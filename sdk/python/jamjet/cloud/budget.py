from __future__ import annotations

import threading

from .exceptions import JamJetBudgetExceeded


class BudgetManager:
    """Track cumulative LLM spend and enforce a cost ceiling."""

    def __init__(self, max_cost_usd: float | None = None) -> None:
        self._max_cost_usd = max_cost_usd
        self._spent: float = 0.0
        self._lock = threading.Lock()

    def record(self, cost_usd: float) -> None:
        """Record actual spend."""
        with self._lock:
            self._spent += cost_usd

    def check_or_raise(self, estimated_cost: float = 0.0) -> None:
        """Raise JamJetBudgetExceeded if adding *estimated_cost* would exceed the limit."""
        if self._max_cost_usd is None:
            return
        with self._lock:
            if self._spent + estimated_cost > self._max_cost_usd:
                raise JamJetBudgetExceeded(spent=self._spent, limit=self._max_cost_usd)

    @property
    def spent(self) -> float:
        with self._lock:
            return self._spent

    @property
    def remaining(self) -> float | None:
        """Remaining budget in USD, or None if no limit is set."""
        if self._max_cost_usd is None:
            return None
        with self._lock:
            return max(0.0, self._max_cost_usd - self._spent)


# ---------------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------------

_budget: BudgetManager = BudgetManager()
_module_lock = threading.Lock()


def get_budget() -> BudgetManager:
    """Return the global budget manager."""
    return _budget


def set_budget(max_cost_usd: float | None) -> BudgetManager:
    """Replace the global budget manager with a new limit."""
    global _budget
    with _module_lock:
        _budget = BudgetManager(max_cost_usd=max_cost_usd)
    return _budget
