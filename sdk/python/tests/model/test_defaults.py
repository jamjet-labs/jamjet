"""Tests for the default model-seam middleware factory (updated for T3-2)."""

from jamjet.model import default_model_middleware
from jamjet.model.budget import BudgetMiddleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware


def test_default_model_middleware_returns_three_elements() -> None:
    """T3-2: chain is now [allowlist, budget, metering] — three elements."""
    chain = default_model_middleware()
    assert len(chain) == 3


def test_default_model_middleware_types() -> None:
    chain = default_model_middleware()
    assert isinstance(chain[0], ModelAllowlistMiddleware)
    assert isinstance(chain[1], BudgetMiddleware)
    assert isinstance(chain[2], MeteringMiddleware)


def test_default_model_middleware_allowlist_is_allow_all() -> None:
    """ModelAllowlistMiddleware(None) means allow-all — the Track 1 default."""
    chain = default_model_middleware()
    allowlist_mw = chain[0]
    assert isinstance(allowlist_mw, ModelAllowlistMiddleware)
    assert allowlist_mw._allowed is None


def test_default_model_middleware_no_governance_budget_is_none() -> None:
    """Without governance, BudgetMiddleware has no budget (no-op pass-through)."""
    chain = default_model_middleware()
    budget_mw = chain[1]
    assert isinstance(budget_mw, BudgetMiddleware)
    assert budget_mw._budget is None


def test_default_model_middleware_with_governance_threads_budget() -> None:
    """Passing governance threads budget into BudgetMiddleware."""
    from jamjet.agents.governance import Budget, normalize_governance

    gov = normalize_governance(budget=Budget(cost_usd=0.50))
    chain = default_model_middleware(governance=gov)
    budget_mw = chain[1]
    assert isinstance(budget_mw, BudgetMiddleware)
    assert budget_mw._budget == Budget(cost_usd=0.50)


def test_default_model_middleware_none_governance_same_as_no_arg() -> None:
    """Explicitly passing governance=None is the same as omitting it."""
    chain = default_model_middleware(governance=None)
    assert len(chain) == 3
    assert isinstance(chain[1], BudgetMiddleware)
    assert chain[1]._budget is None


def test_default_model_middleware_returns_fresh_instances() -> None:
    """Each call returns independent instances so middleware state doesn't leak."""
    a = default_model_middleware()
    b = default_model_middleware()
    assert a[0] is not b[0]
    assert a[1] is not b[1]
    assert a[2] is not b[2]
