"""End-to-end: load a real policy YAML, build the middleware chain, invoke
against a stub terminal mimicking the Anthropic Messages.create() shape.
Asserts the system-prompt-never-scanned contract and the SSN block path."""
import textwrap
import pytest
import yaml
from jamjet.cloud.middleware import build_chain
from jamjet.cloud.middleware.context import (
    call_context_from_anthropic_kwargs,
    anthropic_kwargs_from_call_context,
)
from jamjet.cloud.exceptions import JamJetPIIBlocked


@pytest.fixture
def policy_anthropic_block():
    return yaml.safe_load(textwrap.dedent("""\
        version: 1
        rules:
          - match: "anthropic:*"
            action: redact
            types: [US_SSN]
            on_detect: block
            scope: [messages]
    """))


def test_anthropic_call_with_ssn_is_blocked(monkeypatch, policy_anthropic_block):
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    chain = build_chain(policy_anthropic_block)
    assert len(chain.middlewares) == 1

    kwargs = {
        "model": "claude-haiku-4-5",
        "max_tokens": 128,
        "system": "be helpful",
        "messages": [{"role": "user", "content": "ssn 123-45-6789"}],
    }
    ctx = call_context_from_anthropic_kwargs(kwargs)

    terminal_call_count = 0
    def stub_terminal(c):
        nonlocal terminal_call_count
        terminal_call_count += 1
        rebuilt = anthropic_kwargs_from_call_context(c)
        # Safety contract: SSN must never reach the stub.
        for m in rebuilt.get("messages", []):
            assert "123-45-6789" not in str(m.get("content", ""))
        return "stub-response"

    with pytest.raises(JamJetPIIBlocked):
        chain.run(ctx, terminal=stub_terminal)
    assert terminal_call_count == 0


def test_anthropic_system_prompt_pii_is_not_scanned(monkeypatch):
    """The spec is explicit: system prompts are NEVER scanned, even when they
    contain PII. The call must proceed even with PII in the system kwarg."""
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy = yaml.safe_load(textwrap.dedent("""\
        version: 1
        rules:
          - match: "anthropic:*"
            action: redact
            types: [US_SSN]
            on_detect: block
            scope: [messages]
    """))
    chain = build_chain(policy)

    kwargs = {
        "model": "claude-haiku-4-5",
        "max_tokens": 128,
        "system": "operator ssn 999-99-9999 on file",  # PII in system; MUST be ignored
        "messages": [{"role": "user", "content": "hello"}],
    }
    ctx = call_context_from_anthropic_kwargs(kwargs)

    seen: list = []
    def stub_terminal(c):
        seen.append(anthropic_kwargs_from_call_context(c))
        return "stub-response"

    out = chain.run(ctx, terminal=stub_terminal)
    assert out == "stub-response"
    assert len(seen) == 1
    # System prompt flows through untouched
    assert seen[0]["system"] == "operator ssn 999-99-9999 on file"


def test_anthropic_call_with_replace_redacts_messages(monkeypatch):
    monkeypatch.setenv("JAMJET_MIDDLEWARE_ENABLED", "1")
    policy = yaml.safe_load(textwrap.dedent("""\
        version: 1
        rules:
          - match: "anthropic:claude-haiku-*"
            action: redact
            types: [EMAIL]
            on_detect: replace
            scope: [messages]
    """))
    chain = build_chain(policy)

    kwargs = {
        "model": "claude-haiku-4-5",
        "max_tokens": 128,
        "system": "be helpful",
        "messages": [{"role": "user", "content": "email me alice@example.com"}],
    }
    ctx = call_context_from_anthropic_kwargs(kwargs)

    seen: list = []
    def stub_terminal(c):
        seen.append(anthropic_kwargs_from_call_context(c))
        return "stub-response"

    chain.run(ctx, terminal=stub_terminal)
    assert "alice@example.com" not in seen[0]["messages"][0]["content"]
    assert "[REDACTED:EMAIL]" in seen[0]["messages"][0]["content"]
    # System prompt also untouched (no PII to redact + system never scanned anyway)
    assert seen[0]["system"] == "be helpful"
