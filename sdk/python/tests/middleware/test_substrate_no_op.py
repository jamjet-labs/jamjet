"""Substrate-level guarantee: with no rules in policy.yaml AND no
JAMJET_MIDDLEWARE_ENABLED flag set, the patched LLM call is byte-identical
to what original() returned. This is the load-bearing backwards-compatibility
contract."""


def test_no_op_when_flag_off_and_no_rules(monkeypatch):
    monkeypatch.delenv("JAMJET_MIDDLEWARE_ENABLED", raising=False)
    from jamjet.cloud.middleware import build_chain
    from jamjet.cloud.middleware.context import (
        call_context_from_openai_kwargs,
        openai_kwargs_from_call_context,
    )
    chain = build_chain({"version": 1, "rules": []})
    kwargs = {"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}
    ctx = call_context_from_openai_kwargs(kwargs)

    sentinel = object()
    rebuilt: dict = {}
    def terminal(c):
        rebuilt.update(openai_kwargs_from_call_context(c))
        return sentinel

    out = chain.run(ctx, terminal=terminal)
    assert out is sentinel
    assert rebuilt["model"] == "gpt-4o"
    assert rebuilt["messages"] == [{"role": "user", "content": "hi"}]


def test_no_op_when_flag_on_but_only_tool_call_rules(monkeypatch):
    """Even with the flag enabled, a policy that only contains tool-call
    actions (block/require_approval/audit) must not change LLM-call behaviour."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    from jamjet.cloud.middleware import build_chain
    chain = build_chain({"version": 1, "rules": [
        {"match": "*delete*", "action": "block"},
        {"match": "payments.*", "action": "require_approval"},
    ]})
    assert len(chain.middlewares) == 0
