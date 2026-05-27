from jamjet.cloud.middleware import (
    PreCallMiddleware,
    CallContext,
    MiddlewareOutcome,
)


def test_protocol_and_outcome_types_importable():
    assert PreCallMiddleware is not None
    assert CallContext is not None
    assert MiddlewareOutcome.PASSTHROUGH.value == "passthrough"
    assert MiddlewareOutcome.BLOCKED.value == "blocked"
    assert MiddlewareOutcome.CACHE_HIT.value == "cache_hit"
    assert MiddlewareOutcome.FALLBACK.value == "fallback"


from jamjet.cloud.middleware import Chain, CallContext


def _ctx() -> CallContext:
    return CallContext(provider="openai", model="gpt-4o", messages=[{"role": "user", "content": "hi"}])


def test_empty_chain_invokes_terminal_unchanged():
    chain = Chain(middlewares=[])
    ctx = _ctx()
    out = chain.run(ctx, terminal=lambda c: {"served": c.messages[-1]["content"]})
    assert out == {"served": "hi"}


def test_passthrough_middleware_does_not_short_circuit():
    seen: list[str] = []
    def m(ctx, nxt):
        seen.append("before")
        result = nxt(ctx)
        seen.append("after")
        return result
    chain = Chain(middlewares=[m])
    out = chain.run(_ctx(), terminal=lambda c: "terminal_called")
    assert out == "terminal_called"
    assert seen == ["before", "after"]


def test_short_circuit_skips_terminal():
    terminal_was_called = False
    def terminal(c):
        nonlocal terminal_was_called
        terminal_was_called = True
        return "should-not-see-this"
    def short_circuit(ctx, nxt):
        return "synthesized"
    chain = Chain(middlewares=[short_circuit])
    assert chain.run(_ctx(), terminal=terminal) == "synthesized"
    assert terminal_was_called is False


def test_raise_propagates_through_chain():
    class CustomBlocked(Exception):
        pass
    def blocker(ctx, nxt):
        raise CustomBlocked("block reason")
    chain = Chain(middlewares=[blocker])
    import pytest
    with pytest.raises(CustomBlocked, match="block reason"):
        chain.run(_ctx(), terminal=lambda c: "never")


def test_mutation_propagates_to_terminal():
    def redactor(ctx, nxt):
        ctx.messages = [{"role": "user", "content": "[REDACTED]"}]
        return nxt(ctx)
    chain = Chain(middlewares=[redactor])
    out = chain.run(_ctx(), terminal=lambda c: c.messages[-1]["content"])
    assert out == "[REDACTED]"


def test_fixed_order_executes_pii_then_cache_then_fallback():
    order: list[str] = []
    def m(name):
        def _inner(ctx, nxt):
            order.append(f"{name}:enter")
            try:
                return nxt(ctx)
            finally:
                order.append(f"{name}:exit")
        return _inner
    chain = Chain(middlewares=[m("pii"), m("cache"), m("fallback")])
    chain.run(_ctx(), terminal=lambda c: None)
    assert order == [
        "pii:enter", "cache:enter", "fallback:enter",
        "fallback:exit", "cache:exit", "pii:exit",
    ]
