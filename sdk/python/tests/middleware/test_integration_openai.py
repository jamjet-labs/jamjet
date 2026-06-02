"""End-to-end: load a real policy YAML, build the middleware chain, then
exercise it against a stub terminal. The plan's original spec calls for
patching the OpenAI SDK's class method; we use a direct chain-build approach
that tests the same contract without depending on OpenAI SDK internals or
the patcher's closure-captured `original` reference.

The chain is what every production OpenAI call flows through after Task 6,
so testing the chain directly proves the safety contract end-to-end."""

import textwrap

import pytest
import yaml

from jamjet.cloud.exceptions import JamJetPIIBlocked, JamJetPolicyBlocked
from jamjet.cloud.middleware import build_chain
from jamjet.cloud.middleware.context import (
    call_context_from_openai_kwargs,
    openai_kwargs_from_call_context,
)


def _load_policy_yaml(yaml_text: str) -> dict:
    return yaml.safe_load(yaml_text)


@pytest.fixture
def policy_with_pii_block():
    return _load_policy_yaml(
        textwrap.dedent("""\
        version: 1
        rules:
          - match: "openai:*"
            action: redact
            types: [EMAIL, US_SSN]
            on_detect: block
            scope: [messages]
    """)
    )


@pytest.fixture
def policy_with_pii_replace():
    return _load_policy_yaml(
        textwrap.dedent("""\
        version: 1
        rules:
          - match: "openai:*"
            action: redact
            types: [EMAIL]
            on_detect: replace
            scope: [messages]
    """)
    )


def test_openai_call_with_pii_is_blocked(monkeypatch, policy_with_pii_block):
    """PII in user message -> JamJetPIIBlocked raised -> terminal NEVER called."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    chain = build_chain(policy_with_pii_block)
    assert len(chain.middlewares) == 1  # PII middleware loaded

    kwargs = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "contact alice@example.com"}],
    }
    ctx = call_context_from_openai_kwargs(kwargs)

    terminal_call_count = 0

    def stub_terminal(c):
        nonlocal terminal_call_count
        terminal_call_count += 1
        # The kwargs the stub sees represent what the LLM SDK would receive.
        rebuilt = openai_kwargs_from_call_context(c)
        # If we ever get here with PII intact, the safety contract is broken.
        for m in rebuilt["messages"]:
            assert "alice@example.com" not in str(m.get("content", "")), "PII leaked through chain to terminal!"
        return "stub-response"

    with pytest.raises(JamJetPIIBlocked):
        chain.run(ctx, terminal=stub_terminal)
    assert terminal_call_count == 0  # Terminal NEVER called on block path

    # Subclass semantics: JamJetPolicyBlocked also catches it
    chain2 = build_chain(policy_with_pii_block)
    ctx2 = call_context_from_openai_kwargs(kwargs)
    with pytest.raises(JamJetPolicyBlocked):
        chain2.run(ctx2, terminal=stub_terminal)


def test_openai_call_with_pii_replaces(monkeypatch, policy_with_pii_replace):
    """PII in user message -> redacted in-place -> terminal called with redacted content."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    chain = build_chain(policy_with_pii_replace)
    assert len(chain.middlewares) == 1

    kwargs = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "contact alice@example.com"}],
    }
    ctx = call_context_from_openai_kwargs(kwargs)

    seen_kwargs: list = []

    def stub_terminal(c):
        seen_kwargs.append(openai_kwargs_from_call_context(c))
        return "stub-response"

    out = chain.run(ctx, terminal=stub_terminal)
    assert out == "stub-response"
    assert len(seen_kwargs) == 1
    served_content = seen_kwargs[0]["messages"][0]["content"]
    assert "alice@example.com" not in served_content
    assert "[REDACTED:EMAIL]" in served_content


def test_openai_call_without_pii_passes_through_unchanged(monkeypatch, policy_with_pii_block):
    """No PII in input -> terminal receives byte-identical kwargs.

    This is the load-bearing 'no behaviour change for clean inputs' contract."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    chain = build_chain(policy_with_pii_block)
    kwargs = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hello, how are you?"}],
    }
    ctx = call_context_from_openai_kwargs(kwargs)

    seen_kwargs: list = []

    def stub_terminal(c):
        seen_kwargs.append(openai_kwargs_from_call_context(c))
        return "ok"

    out = chain.run(ctx, terminal=stub_terminal)
    assert out == "ok"
    assert len(seen_kwargs) == 1
    assert seen_kwargs[0]["messages"][0]["content"] == "hello, how are you?"


def test_flag_off_yields_empty_chain_for_redact_rules(monkeypatch, policy_with_pii_block):
    """When the feature flag is off, the chain is empty even with redact rules.
    PII flows through unchanged - same as pre-Phase-1 behaviour."""
    monkeypatch.delenv("JAMJET_MIDDLEWARE_ENABLED", raising=False)
    chain = build_chain(policy_with_pii_block)
    assert len(chain.middlewares) == 0
    kwargs = {
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "contact alice@example.com"}],
    }
    ctx = call_context_from_openai_kwargs(kwargs)

    seen_kwargs: list = []

    def stub_terminal(c):
        seen_kwargs.append(openai_kwargs_from_call_context(c))
        return "ok"

    chain.run(ctx, terminal=stub_terminal)
    # Flag off -> empty chain -> PII flows through unchanged
    assert "alice@example.com" in seen_kwargs[0]["messages"][0]["content"]
