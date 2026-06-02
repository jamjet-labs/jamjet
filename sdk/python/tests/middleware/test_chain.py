from jamjet.cloud.middleware import (
    CallContext,
    MiddlewareOutcome,
    PreCallMiddleware,
)


def test_protocol_and_outcome_types_importable():
    assert PreCallMiddleware is not None
    assert CallContext is not None
    assert MiddlewareOutcome.PASSTHROUGH.value == "passthrough"
    assert MiddlewareOutcome.BLOCKED.value == "blocked"
    assert MiddlewareOutcome.CACHE_HIT.value == "cache_hit"
    assert MiddlewareOutcome.FALLBACK.value == "fallback"


from jamjet.cloud.middleware import Chain  # noqa: E402


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
    class CustomBlockedError(Exception):
        pass

    def blocker(ctx, nxt):
        raise CustomBlockedError("block reason")

    chain = Chain(middlewares=[blocker])
    import pytest

    with pytest.raises(CustomBlockedError, match="block reason"):
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
        "pii:enter",
        "cache:enter",
        "fallback:enter",
        "fallback:exit",
        "cache:exit",
        "pii:exit",
    ]


from jamjet.cloud.middleware import build_chain  # noqa: E402


def test_build_chain_returns_empty_when_flag_disabled(monkeypatch):
    monkeypatch.delenv("JAMJET_MIDDLEWARE_ENABLED", raising=False)
    policy = {
        "version": 1,
        "rules": [
            {"match": "openai:*", "action": "redact", "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"]}
        ],
    }
    chain = build_chain(policy)
    assert len(chain.middlewares) == 0  # flag off -> chain is empty -> no behaviour change


def test_build_chain_returns_empty_when_no_middleware_rules(monkeypatch):
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy = {
        "version": 1,
        "rules": [
            {"match": "*delete*", "action": "block"},  # tool-call rule, NOT a middleware rule
        ],
    }
    chain = build_chain(policy)
    assert len(chain.middlewares) == 0


def test_build_chain_skips_unknown_action(monkeypatch):
    """`cache` and `fallback` are reserved for Phases 2/3; build_chain must
    silently skip them in Phase 1 so a forward-compatible policy.yaml doesn't
    crash today."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy = {
        "version": 1,
        "rules": [
            {"match": "openai:*", "action": "cache", "ttl": 60},
            {
                "match": "openai:*",
                "action": "fallback",
                "on": [{"http_status": [503]}],
                "chain": ["openai:gpt-4o-mini"],
            },
        ],
    }
    chain = build_chain(policy)
    assert len(chain.middlewares) == 0


# ---------------------------------------------------------------------------
# Task 7: configure() builds the chain from a loaded policy.yaml
# ---------------------------------------------------------------------------

import pytest  # noqa: E402  (pre-existing pattern in this test file)


@pytest.mark.xfail(
    reason="PIIMiddleware (jamjet.cloud.middleware.pii) ships in Task 11; "
    "until then build_chain raises ImportError for redact rules",
    strict=False,
)
def test_configure_builds_chain_from_loaded_policy(monkeypatch, tmp_path):
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy_yaml = tmp_path / "policy.yaml"
    policy_yaml.write_text(
        "version: 1\n"
        "rules:\n"
        '  - { match: "openai:*", action: redact, types: [EMAIL], '
        "on_detect: block, scope: [messages] }\n"
    )
    import jamjet.cloud as cloud

    cloud.configure(policy_path=str(policy_yaml), telemetry=False, auto_patch=False)

    from jamjet.cloud.patcher import _runtime_state

    state = _runtime_state()
    assert len(state.middleware_chain.middlewares) == 1


def test_configure_with_no_middleware_rules_yields_empty_chain(monkeypatch, tmp_path):
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy_yaml = tmp_path / "policy.yaml"
    policy_yaml.write_text("version: 1\nrules: []\n")
    import jamjet.cloud as cloud

    cloud.configure(policy_path=str(policy_yaml), telemetry=False, auto_patch=False)

    from jamjet.cloud.patcher import _runtime_state

    assert len(_runtime_state().middleware_chain.middlewares) == 0
