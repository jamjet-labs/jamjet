from jamjet.cloud.middleware.pii import RegexDetector, PIIDetection


def test_regex_detector_finds_email():
    d = RegexDetector(types=["EMAIL"])
    detections = d.scan("contact alice@example.com for details")
    assert len(detections) == 1
    assert detections[0].type == "EMAIL"
    assert detections[0].value == "alice@example.com"


def test_regex_detector_finds_us_ssn():
    d = RegexDetector(types=["US_SSN"])
    detections = d.scan("ssn 123-45-6789 on file")
    assert any(x.type == "US_SSN" and x.value == "123-45-6789" for x in detections)


def test_regex_detector_finds_credit_card():
    d = RegexDetector(types=["CREDIT_CARD"])
    detections = d.scan("card 4111 1111 1111 1111 expires 12/26")
    assert any(x.type == "CREDIT_CARD" for x in detections)


def test_regex_detector_ignores_unrequested_types():
    d = RegexDetector(types=["EMAIL"])
    detections = d.scan("ssn 123-45-6789 email alice@example.com")
    assert len(detections) == 1
    assert detections[0].type == "EMAIL"


def test_regex_detector_empty_input_returns_empty():
    d = RegexDetector(types=["EMAIL"])
    assert d.scan("") == []
    assert d.scan("no pii here") == []


def test_redact_in_place_substitutes_tokens():
    d = RegexDetector(types=["EMAIL", "US_SSN"])
    out = d.redact("contact alice@example.com or ssn 123-45-6789")
    assert "alice@example.com" not in out
    assert "123-45-6789" not in out
    assert "[REDACTED:EMAIL]" in out
    assert "[REDACTED:US_SSN]" in out


import logging
import pytest


@pytest.fixture(autouse=True)
def _pii_logs_at_warning(caplog):
    caplog.set_level(logging.WARNING, logger="jamjet.cloud.middleware.pii")


def test_presidio_detector_imports_only_when_extras_installed():
    """PresidioDetector should be importable; instantiation should raise a
    clear error if presidio-analyzer is not installed (instead of an opaque
    ModuleNotFoundError deep in the call stack)."""
    from jamjet.cloud.middleware.pii import PresidioDetector
    try:
        import presidio_analyzer  # noqa: F401
        has_presidio = True
    except ImportError:
        has_presidio = False
    if not has_presidio:
        with pytest.raises(ImportError, match=r"jamjet\[pii\]"):
            PresidioDetector(types=["EMAIL"])
    else:
        # Smoke: instantiate and run one scan
        d = PresidioDetector(types=["EMAIL"])
        detections = d.scan("contact alice@example.com")
        assert any(x.type == "EMAIL" for x in detections)


from jamjet.cloud.middleware import CallContext
from jamjet.cloud.middleware.pii import PIIMiddleware


def _ctx_with_email() -> CallContext:
    return CallContext(
        provider="openai", model="gpt-4o",
        messages=[{"role": "user", "content": "email alice@example.com"}],
        tools=[],
        system="be helpful",
    )


def test_pii_block_raises_jamjet_pii_blocked():
    from jamjet.cloud.exceptions import JamJetPIIBlocked
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"],
    }])
    with pytest.raises(JamJetPIIBlocked) as ei:
        mw(_ctx_with_email(), next=lambda c: "should-not-call-terminal")
    assert ei.value.rule_pattern == "openai:*"
    assert ei.value.types_detected == ["EMAIL"]
    # Sanitized — TYPES + COUNT only, never the raw PII value.
    assert "alice@example.com" not in str(ei.value)


def test_pii_block_also_caught_by_jamjet_policy_blocked():
    """Subclass semantics: existing `except JamJetPolicyBlocked` handlers
    must still catch PII blocks."""
    from jamjet.cloud.exceptions import JamJetPolicyBlocked
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"],
    }])
    with pytest.raises(JamJetPolicyBlocked):
        mw(_ctx_with_email(), next=lambda c: "should-not-call-terminal")


def test_pii_block_ignores_non_matching_rules():
    """Rule matches `anthropic:*` but call is `openai:*` — pass through."""
    mw = PIIMiddleware(rules=[{
        "match": "anthropic:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"],
    }])
    out = mw(_ctx_with_email(), next=lambda c: "passed-through")
    assert out == "passed-through"


def test_pii_block_skips_system_prompts():
    """Even with a matching rule including scope=[messages], the system
    prompt must never be scanned."""
    ctx = CallContext(
        provider="openai", model="gpt-4o",
        messages=[{"role": "user", "content": "no pii here"}],
        system="ops contact: alice@example.com",   # PII in system; must be ignored
    )
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"],
    }])
    out = mw(ctx, next=lambda c: "passed-through")
    assert out == "passed-through"


def test_pii_block_scans_tool_descriptions_when_scope_includes_tools():
    ctx = CallContext(
        provider="openai", model="gpt-4o",
        messages=[{"role": "user", "content": "no pii"}],
        tools=[{"type": "function", "function": {
            "name": "lookup", "description": "lookup by email like alice@example.com",
        }}],
    )
    from jamjet.cloud.exceptions import JamJetPIIBlocked
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["tools"],
    }])
    with pytest.raises(JamJetPIIBlocked):
        mw(ctx, next=lambda c: "should-not-pass-through")


def test_pii_replace_substitutes_tokens_and_calls_terminal():
    seen_messages: list = []
    def terminal(c):
        seen_messages.append(c.messages[0]["content"])
        return "terminal-response"

    ctx = CallContext(
        provider="openai", model="gpt-4o",
        messages=[{"role": "user", "content": "email alice@example.com"}],
    )
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "replace", "scope": ["messages"],
    }])
    out = mw(ctx, next=terminal)
    assert out == "terminal-response"
    assert seen_messages == ["email [REDACTED:EMAIL]"]
    # Original ctx must reflect the mutation (the patcher rebuilds vendor kwargs from ctx).
    assert ctx.messages[0]["content"] == "email [REDACTED:EMAIL]"


def test_pii_replace_handles_multi_part_content():
    ctx = CallContext(
        provider="openai", model="gpt-4o",
        messages=[{"role": "user", "content": [
            {"type": "text", "text": "ssn 123-45-6789"},
            {"type": "image_url", "image_url": {"url": "http://example.com/img.png"}},
            {"type": "text", "text": "another email bob@example.com"},
        ]}],
    )
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL", "US_SSN"], "on_detect": "replace", "scope": ["messages"],
    }])
    out = mw(ctx, next=lambda c: "ok")
    assert out == "ok"
    parts = ctx.messages[0]["content"]
    assert "[REDACTED:US_SSN]" in parts[0]["text"]
    assert "[REDACTED:EMAIL]" in parts[2]["text"]
    # Non-text parts are untouched
    assert parts[1]["image_url"]["url"] == "http://example.com/img.png"


def test_pii_detector_error_fails_open(caplog):
    """A bug in the detector must NEVER break the user's LLM call."""
    from jamjet.cloud.middleware.pii import PIIDetector

    class BrokenDetector(PIIDetector):
        def scan(self, text): raise RuntimeError("detector blew up")

    from jamjet.cloud.middleware import CallContext
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL"], "on_detect": "block", "scope": ["messages"],
    }])
    # Override the compiled detector for this rule
    mw._compiled = [(mw._compiled[0][0], BrokenDetector())]
    ctx = CallContext(provider="openai", model="gpt-4o",
                      messages=[{"role": "user", "content": "hi alice@example.com"}])
    out = mw(ctx, next=lambda c: "served-anyway")
    assert out == "served-anyway"
    assert any("pii detector error" in r.message for r in caplog.records)
