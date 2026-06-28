"""Tests for BudgetMiddleware and BudgetExceededError (T3-2).

Adversarial dual coverage for every enforcement path:
- Under-budget calls PROCEED (backend IS called).
- At/over-budget calls are DENIED before the provider (backend NOT called).
- Token budget and cost budget each have their own dual.
- No-budget (None) is a complete no-op: all calls pass through.
- Error message names both the limit and the consumed amount.
- BudgetExceededError is a ModelDeniedError subclass (fail-closed family).
"""

from __future__ import annotations

import pytest

from jamjet.agents.governance import Budget
from jamjet.model.budget import BudgetMiddleware
from jamjet.model.middleware import BudgetExceededError, ModelDeniedError
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse, parse_model_ref

# ---------------------------------------------------------------------------
# Test helpers
# ---------------------------------------------------------------------------


class FakeBackend:
    """Fake backend. Counts calls and returns a fixed response; never hits a provider."""

    def __init__(self, *, cost_usd: float = 0.05, input_tokens: int = 5, output_tokens: int = 5) -> None:
        self.call_count = 0
        self._cost_usd = cost_usd
        self._input_tokens = input_tokens
        self._output_tokens = output_tokens

    async def complete(self, request: ModelRequest) -> ModelResponse:
        self.call_count += 1
        return ModelResponse(
            message=object(),
            input_tokens=self._input_tokens,
            output_tokens=self._output_tokens,
            cost_usd=self._cost_usd,
        )


def _req() -> ModelRequest:
    return ModelRequest(ref=parse_model_ref("anthropic/claude-opus-4-8"), messages=[])


# ---------------------------------------------------------------------------
# BudgetExceededError — shape and error hierarchy
# ---------------------------------------------------------------------------


class TestBudgetExceededError:
    def test_is_model_denied_error(self) -> None:
        """BudgetExceededError is-a ModelDeniedError (fail-closed family)."""
        err = BudgetExceededError(limit_usd=0.10, limit_tokens=None, consumed_usd=0.12, consumed_tokens=0)
        assert isinstance(err, ModelDeniedError)

    def test_code_is_budget_exceeded(self) -> None:
        err = BudgetExceededError(limit_usd=0.10, limit_tokens=None, consumed_usd=0.12, consumed_tokens=0)
        assert err.code == "budget_exceeded"

    def test_message_contains_limit_and_consumed_cost(self) -> None:
        err = BudgetExceededError(limit_usd=0.10, limit_tokens=None, consumed_usd=0.12, consumed_tokens=0)
        msg = str(err)
        # Both the consumed amount and the limit must appear in the message.
        assert "0.12" in msg or "0.120000" in msg
        assert "0.10" in msg or "0.100000" in msg

    def test_message_contains_limit_and_consumed_tokens(self) -> None:
        err = BudgetExceededError(limit_usd=None, limit_tokens=100, consumed_usd=0.0, consumed_tokens=150)
        msg = str(err)
        assert "150" in msg
        assert "100" in msg

    def test_fields_accessible(self) -> None:
        err = BudgetExceededError(limit_usd=1.0, limit_tokens=500, consumed_usd=1.5, consumed_tokens=600)
        assert err.limit_usd == pytest.approx(1.0)
        assert err.limit_tokens == 500
        assert err.consumed_usd == pytest.approx(1.5)
        assert err.consumed_tokens == 600

    def test_cost_only_budget_message(self) -> None:
        """Message must name the cost figures when token cap is absent."""
        err = BudgetExceededError(limit_usd=0.50, limit_tokens=None, consumed_usd=0.50, consumed_tokens=100)
        msg = str(err)
        assert "cost" in msg.lower() or "0.50" in msg or "0.500000" in msg

    def test_token_only_budget_message(self) -> None:
        """Message must name the token figures when cost cap is absent."""
        err = BudgetExceededError(limit_usd=None, limit_tokens=200, consumed_usd=0.0, consumed_tokens=200)
        msg = str(err)
        assert "token" in msg.lower() or "200" in msg


# ---------------------------------------------------------------------------
# BudgetMiddleware unit tests (hooks tested directly, no Model)
# ---------------------------------------------------------------------------


class TestBudgetMiddlewareNoBudget:
    async def test_before_passes_through(self) -> None:
        """No budget -> before() returns the request unchanged (no enforcement)."""
        mw = BudgetMiddleware(None)
        req = _req()
        out = await mw.before(req)
        assert out is req

    async def test_after_does_not_accumulate(self) -> None:
        """No budget -> after() does not accumulate (no state to corrupt)."""
        mw = BudgetMiddleware(None)
        resp = ModelResponse(message=object(), input_tokens=50, output_tokens=50, cost_usd=0.10)
        await mw.after(_req(), resp)
        assert mw.consumed_tokens == 0
        assert mw.consumed_cost_usd == pytest.approx(0.0)


class TestBudgetMiddlewareCostBudget:
    async def test_first_call_under_budget_proceeds(self) -> None:
        """No spend yet -> before() does not raise (adversarial dual: happy path)."""
        mw = BudgetMiddleware(Budget(cost_usd=0.10))
        req = _req()
        out = await mw.before(req)
        assert out is req

    async def test_at_budget_denies_next_call(self) -> None:
        """After spending exactly the limit, the next before() call is DENIED."""
        mw = BudgetMiddleware(Budget(cost_usd=0.10))
        # Simulate a completed call that spent exactly the limit.
        resp = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.10)
        await mw.after(_req(), resp)
        assert mw.consumed_cost_usd == pytest.approx(0.10)

        with pytest.raises(BudgetExceededError) as exc_info:
            await mw.before(_req())
        assert exc_info.value.code == "budget_exceeded"
        assert exc_info.value.limit_usd == pytest.approx(0.10)
        assert exc_info.value.consumed_usd == pytest.approx(0.10)

    async def test_over_budget_denies_next_call(self) -> None:
        """After spending MORE than the limit, the next before() call is DENIED."""
        mw = BudgetMiddleware(Budget(cost_usd=0.10))
        resp = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.15)
        await mw.after(_req(), resp)

        with pytest.raises(BudgetExceededError):
            await mw.before(_req())

    async def test_accumulates_cost_across_calls(self) -> None:
        mw = BudgetMiddleware(Budget(cost_usd=0.50))
        r1 = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.07)
        r2 = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.08)
        await mw.after(_req(), r1)
        await mw.after(_req(), r2)
        assert mw.consumed_cost_usd == pytest.approx(0.15)


class TestBudgetMiddlewareTokenBudget:
    async def test_first_call_under_token_budget_proceeds(self) -> None:
        """No tokens spent -> before() does not raise."""
        mw = BudgetMiddleware(Budget(tokens=100))
        req = _req()
        out = await mw.before(req)
        assert out is req

    async def test_at_token_budget_denies_next_call(self) -> None:
        """After spending exactly the token limit, next before() call is DENIED."""
        mw = BudgetMiddleware(Budget(tokens=100))
        resp = ModelResponse(message=object(), input_tokens=60, output_tokens=40, cost_usd=0.0)
        await mw.after(_req(), resp)
        assert mw.consumed_tokens == 100

        with pytest.raises(BudgetExceededError) as exc_info:
            await mw.before(_req())
        err = exc_info.value
        assert err.limit_tokens == 100
        assert err.consumed_tokens == 100

    async def test_over_token_budget_denies(self) -> None:
        mw = BudgetMiddleware(Budget(tokens=100))
        resp = ModelResponse(message=object(), input_tokens=70, output_tokens=50, cost_usd=0.0)
        await mw.after(_req(), resp)
        assert mw.consumed_tokens == 120

        with pytest.raises(BudgetExceededError):
            await mw.before(_req())

    async def test_tokens_are_input_plus_output(self) -> None:
        """Accumulated tokens = input_tokens + output_tokens per call."""
        mw = BudgetMiddleware(Budget(tokens=1000))
        resp = ModelResponse(message=object(), input_tokens=30, output_tokens=20, cost_usd=0.0)
        await mw.after(_req(), resp)
        assert mw.consumed_tokens == 50


class TestBudgetMiddlewareBothCaps:
    async def test_cost_exceeded_triggers_denial_when_tokens_ok(self) -> None:
        """Either cap exceeded is enough to deny — cost cap hit here."""
        mw = BudgetMiddleware(Budget(tokens=1000, cost_usd=0.10))
        # Spend exactly the cost limit; tokens are well under.
        resp = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.10)
        await mw.after(_req(), resp)

        with pytest.raises(BudgetExceededError) as exc_info:
            await mw.before(_req())
        err = exc_info.value
        assert err.limit_usd == pytest.approx(0.10)
        assert err.consumed_usd == pytest.approx(0.10)

    async def test_token_exceeded_triggers_denial_when_cost_ok(self) -> None:
        """Either cap exceeded is enough to deny — token cap hit here."""
        mw = BudgetMiddleware(Budget(tokens=10, cost_usd=100.0))
        # Spend exactly the token limit; cost is well under.
        resp = ModelResponse(message=object(), input_tokens=5, output_tokens=5, cost_usd=0.01)
        await mw.after(_req(), resp)

        with pytest.raises(BudgetExceededError):
            await mw.before(_req())


# ---------------------------------------------------------------------------
# Integration via Model — backend NOT called on denial (adversarial dual)
# ---------------------------------------------------------------------------


class TestBudgetMiddlewareViaModel:
    async def test_under_cost_budget_backend_is_called(self) -> None:
        """Happy path: under budget -> call proceeds, backend IS called."""
        backend = FakeBackend(cost_usd=0.05, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(cost_usd=0.10))])
        await model.complete(_req())
        assert backend.call_count == 1

    async def test_at_cost_budget_next_call_denied_backend_not_called(self) -> None:
        """At budget: next call DENIED; backend is NOT called (adversarial dual)."""
        # First call exhausts the budget exactly.
        backend = FakeBackend(cost_usd=0.10, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(cost_usd=0.10))])

        await model.complete(_req())
        assert backend.call_count == 1  # first call succeeded

        # Second call: accumulated cost == 0.10 >= limit 0.10 -> denied before backend.
        with pytest.raises(BudgetExceededError):
            await model.complete(_req())
        assert backend.call_count == 1, "backend must NOT be called on the denied call"

    async def test_under_token_budget_backend_is_called(self) -> None:
        """Happy path: under token budget -> call proceeds, backend IS called."""
        backend = FakeBackend(cost_usd=0.0, input_tokens=4, output_tokens=4)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(tokens=20))])
        await model.complete(_req())
        assert backend.call_count == 1

    async def test_at_token_budget_next_call_denied_backend_not_called(self) -> None:
        """Token budget exhausted: next call DENIED; backend is NOT called."""
        # Each call uses 10 tokens (5+5); budget is 10.
        backend = FakeBackend(cost_usd=0.0, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(tokens=10))])

        await model.complete(_req())
        assert backend.call_count == 1

        with pytest.raises(BudgetExceededError):
            await model.complete(_req())
        assert backend.call_count == 1, "backend must NOT be called on the denied call"

    async def test_no_budget_allows_all_calls_backend_called_each_time(self) -> None:
        """No budget (None): all calls proceed; backend called every time."""
        backend = FakeBackend(cost_usd=1.0, input_tokens=100, output_tokens=100)
        model = Model(backend=backend, middleware=[BudgetMiddleware(None)])
        for _ in range(5):
            await model.complete(_req())
        assert backend.call_count == 5

    async def test_error_message_names_limit_and_consumed(self) -> None:
        """Error message must name both the limit and the consumed amount."""
        backend = FakeBackend(cost_usd=0.10, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(cost_usd=0.10))])
        await model.complete(_req())

        with pytest.raises(BudgetExceededError) as exc_info:
            await model.complete(_req())
        err = exc_info.value
        msg = str(err)
        # Limit and consumed must appear.
        assert "0.10" in msg or "0.100000" in msg
        assert err.consumed_usd == pytest.approx(0.10)
        assert err.limit_usd == pytest.approx(0.10)

    async def test_budget_exceeded_error_is_model_denied_error(self) -> None:
        """Catching ModelDeniedError catches BudgetExceededError (fail-closed family)."""
        backend = FakeBackend(cost_usd=0.10, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(cost_usd=0.10))])
        await model.complete(_req())

        with pytest.raises(ModelDeniedError):
            await model.complete(_req())

    async def test_multiple_calls_accumulate_then_deny(self) -> None:
        """Three calls each at $0.03 against a $0.09 limit: third call hits the cap."""
        backend = FakeBackend(cost_usd=0.03, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=[BudgetMiddleware(Budget(cost_usd=0.09))])

        await model.complete(_req())  # spent: 0.03
        await model.complete(_req())  # spent: 0.06
        await model.complete(_req())  # spent: 0.09
        assert backend.call_count == 3

        # Fourth call: 0.09 >= 0.09 -> denied.
        with pytest.raises(BudgetExceededError):
            await model.complete(_req())
        assert backend.call_count == 3, "fourth call must not reach backend"


# ---------------------------------------------------------------------------
# default_model_middleware integration — budget threads through correctly
# ---------------------------------------------------------------------------


class TestDefaultMiddlewareIntegration:
    async def test_governance_budget_enforced_via_default_middleware(self) -> None:
        """BudgetMiddleware from default_model_middleware() enforces the governance budget."""
        from jamjet.agents.governance import normalize_governance
        from jamjet.model.defaults import default_model_middleware

        gov = normalize_governance(budget=Budget(cost_usd=0.10))
        backend = FakeBackend(cost_usd=0.10, input_tokens=5, output_tokens=5)
        model = Model(backend=backend, middleware=default_model_middleware(governance=gov))

        await model.complete(_req())  # spends the budget

        with pytest.raises(BudgetExceededError):
            await model.complete(_req())
        assert backend.call_count == 1

    async def test_no_governance_budget_is_no_op(self) -> None:
        """default_model_middleware() without governance -> no budget enforcement."""
        from jamjet.model.defaults import default_model_middleware

        backend = FakeBackend(cost_usd=5.0, input_tokens=1000, output_tokens=1000)
        model = Model(backend=backend, middleware=default_model_middleware())

        for _ in range(3):
            await model.complete(_req())
        assert backend.call_count == 3
