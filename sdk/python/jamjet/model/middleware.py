"""The model-call middleware chain: the seam's enforcement point.

Track 1 ships the protocol, a no-op base, and the allowlist. Budget-cap and
PII-redaction middleware land in Track 3 against this same protocol.
"""

from __future__ import annotations

from typing import Protocol, runtime_checkable

from jamjet.model.types import ModelRequest, ModelResponse


class ModelDeniedError(Exception):
    """Raised by a ``before`` hook to deny a model call before it reaches a provider."""

    def __init__(self, reason: str, *, code: str = "denied") -> None:
        super().__init__(reason)
        self.reason = reason
        self.code = code


class BudgetExceededError(ModelDeniedError):
    """Raised by ``BudgetMiddleware`` when accumulated spend has reached the budget.

    Fail-closed: the call is denied *before* it reaches the provider.
    Both the limit and the consumed amount are named in the message so callers
    can surface the exact figures to the user or an audit log.
    """

    def __init__(
        self,
        *,
        limit_usd: float | None,
        limit_tokens: int | None,
        consumed_usd: float,
        consumed_tokens: int,
    ) -> None:
        parts: list[str] = []
        if limit_usd is not None:
            parts.append(f"cost ${consumed_usd:.6f} >= limit ${limit_usd:.6f}")
        if limit_tokens is not None:
            parts.append(f"tokens {consumed_tokens} >= limit {limit_tokens}")
        reason = "budget exceeded: " + "; ".join(parts) if parts else "budget exceeded"
        super().__init__(reason, code="budget_exceeded")
        self.limit_usd = limit_usd
        self.limit_tokens = limit_tokens
        self.consumed_usd = consumed_usd
        self.consumed_tokens = consumed_tokens


@runtime_checkable
class ModelMiddleware(Protocol):
    async def before(self, request: ModelRequest) -> ModelRequest: ...
    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse: ...


class BaseModelMiddleware:
    """No-op base so middleware override only the hook they need."""

    async def before(self, request: ModelRequest) -> ModelRequest:
        return request

    async def after(self, request: ModelRequest, response: ModelResponse) -> ModelResponse:
        return response


def _glob_match(pattern: str, value: str) -> bool:
    """``*``/``?`` glob, matching ``glob_match`` in ``runtime/policy/src/lib.rs``.

    Deliberately NOT :func:`fnmatch.fnmatchcase`: fnmatch also honours ``[seq]``
    character classes, which the Rust matcher treats as literal characters. Using
    it would fix one divergence by introducing another, subtler one — and the
    whole point of this function is that both sides agree.

    Iterative rather than recursive, so an adversarial pattern cannot blow the
    stack or backtrack exponentially on a long model name.
    """
    p = v = 0
    star = -1
    resume = 0
    while v < len(value):
        if p < len(pattern) and pattern[p] in (value[v], "?"):
            p += 1
            v += 1
        elif p < len(pattern) and pattern[p] == "*":
            star = p
            resume = v
            p += 1
        elif star >= 0:
            p = star + 1
            resume += 1
            v = resume
        else:
            return False
    while p < len(pattern) and pattern[p] == "*":
        p += 1
    return p == len(pattern)


def model_allowlist_matches(pattern: str, provider: str, litellm_model: str) -> bool:
    """Does one allowlist entry admit this model?

    Mirrors ``model_matches`` in ``runtime/policy/src/lib.rs`` exactly, because
    the SAME allowlist is evaluated here for an in-process run and there for a
    durable one. When the two disagree, a policy means different things depending
    on which transport happened to run it — and the disagreement surfaces in
    production, since development usually stays in-process.

    Entries are globs (``*``/``?``) matched against the full ``provider/model``
    reference, and an entry with no ``/`` is also matched against the provider
    alone, so ``"anthropic"`` admits every Anthropic model.
    """
    if _glob_match(pattern, litellm_model):
        return True
    if "/" in pattern:
        return False
    return _glob_match(pattern, provider)


class ModelAllowlistMiddleware(BaseModelMiddleware):
    """Deny model calls whose provider or full model string is not allowed.

    ``allowed=None`` allows everything (the Track 1 default; Track 3 wires the
    real policy-derived allowlist).

    Entries are globs, matched against the full reference or the provider alone —
    the same rule the durable engine applies. Membership used to be exact, so
    ``"anthropic/*"`` matched nothing and ``"*"`` denied every model, while both
    were accepted durable-side.
    """

    def __init__(self, allowed: set[str] | None) -> None:
        self._allowed = allowed

    async def before(self, request: ModelRequest) -> ModelRequest:
        if self._allowed is None:
            return request
        ref = request.ref
        if any(model_allowlist_matches(pattern, ref.provider, ref.litellm_model) for pattern in self._allowed):
            return request
        raise ModelDeniedError(
            f"model {ref.litellm_model!r} is not in the allowlist",
            code="model_not_allowed",
        )
