"""Tests for the default model-seam middleware factory (updated for T3-3).

Chain order when pii=True (the default):
  [ModelAllowlistMiddleware, PiiRedactionMiddleware, BudgetMiddleware, MeteringMiddleware]

Chain order when pii=False:
  [ModelAllowlistMiddleware, BudgetMiddleware, MeteringMiddleware]
"""

from jamjet.agents.governance import Budget, normalize_governance
from jamjet.model import default_model_middleware
from jamjet.model.budget import BudgetMiddleware
from jamjet.model.metering import MeteringMiddleware
from jamjet.model.middleware import ModelAllowlistMiddleware
from jamjet.model.pii import PiiRedactionMiddleware

# ---------------------------------------------------------------------------
# Default chain (pii=True, no explicit governance)
# ---------------------------------------------------------------------------


def test_default_model_middleware_returns_four_elements() -> None:
    """T3-3: default chain is [allowlist, pii, budget, metering] -- four elements."""
    chain = default_model_middleware()
    assert len(chain) == 4


def test_default_model_middleware_types() -> None:
    chain = default_model_middleware()
    assert isinstance(chain[0], ModelAllowlistMiddleware)
    assert isinstance(chain[1], PiiRedactionMiddleware)
    assert isinstance(chain[2], BudgetMiddleware)
    assert isinstance(chain[3], MeteringMiddleware)


def test_default_model_middleware_allowlist_is_allow_all() -> None:
    """ModelAllowlistMiddleware(None) means allow-all -- the Track 1 default."""
    chain = default_model_middleware()
    allowlist_mw = chain[0]
    assert isinstance(allowlist_mw, ModelAllowlistMiddleware)
    assert allowlist_mw._allowed is None


def test_default_model_middleware_no_governance_budget_is_none() -> None:
    """Without governance, BudgetMiddleware has no budget (no-op pass-through)."""
    chain = default_model_middleware()
    budget_mw = chain[2]
    assert isinstance(budget_mw, BudgetMiddleware)
    assert budget_mw._budget is None


def test_default_model_middleware_with_governance_threads_budget() -> None:
    """Passing governance threads budget into BudgetMiddleware."""
    gov = normalize_governance(budget=Budget(cost_usd=0.50))
    chain = default_model_middleware(governance=gov)
    budget_mw = chain[2]
    assert isinstance(budget_mw, BudgetMiddleware)
    assert budget_mw._budget == Budget(cost_usd=0.50)


def test_default_model_middleware_none_governance_same_as_no_arg() -> None:
    """Explicitly passing governance=None is the same as omitting it."""
    chain = default_model_middleware(governance=None)
    assert len(chain) == 4
    assert isinstance(chain[1], PiiRedactionMiddleware)
    assert isinstance(chain[2], BudgetMiddleware)
    assert chain[2]._budget is None


def test_default_model_middleware_returns_fresh_instances() -> None:
    """Each call returns independent instances so middleware state doesn't leak."""
    a = default_model_middleware()
    b = default_model_middleware()
    assert a[0] is not b[0]
    assert a[1] is not b[1]
    assert a[2] is not b[2]
    assert a[3] is not b[3]


# ---------------------------------------------------------------------------
# pii=False: PiiRedactionMiddleware excluded -> 3-element chain
# ---------------------------------------------------------------------------


def test_pii_false_omits_pii_middleware() -> None:
    """governance.pii=False -> PiiRedactionMiddleware excluded from the chain."""
    gov = normalize_governance(pii=False)
    chain = default_model_middleware(governance=gov)
    assert len(chain) == 3
    assert isinstance(chain[0], ModelAllowlistMiddleware)
    assert isinstance(chain[1], BudgetMiddleware)
    assert isinstance(chain[2], MeteringMiddleware)
    assert not any(isinstance(mw, PiiRedactionMiddleware) for mw in chain)


def test_pii_true_includes_pii_middleware() -> None:
    """governance.pii=True (default) -> PiiRedactionMiddleware at index 1."""
    gov = normalize_governance(pii=True)
    chain = default_model_middleware(governance=gov)
    assert len(chain) == 4
    assert isinstance(chain[1], PiiRedactionMiddleware)


def test_default_pii_on_without_governance() -> None:
    """Without governance, PII is ON by default (safe default)."""
    chain = default_model_middleware()
    assert any(isinstance(mw, PiiRedactionMiddleware) for mw in chain)
