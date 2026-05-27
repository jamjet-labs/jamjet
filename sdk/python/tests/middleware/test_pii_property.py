"""Property: for any input containing or not containing PII, after the PII
middleware fires, the terminal call either NEVER runs (block path) or runs
with a CallContext that contains zero detector hits (replace path)."""
from hypothesis import given, strategies as st
from jamjet.cloud.middleware import CallContext
from jamjet.cloud.middleware.pii import (
    PIIMiddleware, RegexDetector,
)
from jamjet.cloud.exceptions import JamJetPIIBlocked


@given(
    base_text=st.text(alphabet=st.characters(blacklist_categories=("Cc",)),
                      min_size=0, max_size=200),
)
def test_block_mode_never_passes_pii_to_terminal(base_text):
    """If the middleware doesn't raise, the terminal was called — and the
    terminal must have received content with no detector hits.
    If the middleware raises, the terminal was never called."""
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL", "US_SSN", "PHONE_NUMBER"],
        "on_detect": "block", "scope": ["messages"],
    }])
    ctx = CallContext(provider="openai", model="gpt-4o",
                      messages=[{"role": "user", "content": base_text}])
    terminal_called_with = []
    try:
        mw(ctx, next=lambda c: terminal_called_with.append(c) or "ok")
    except JamJetPIIBlocked:
        # If we blocked, terminal must NOT have been called.
        assert terminal_called_with == []
    else:
        # If we passed through, the input must contain NO detector matches.
        d = RegexDetector(types=["EMAIL", "US_SSN", "PHONE_NUMBER"])
        for c in terminal_called_with:
            for m in c.messages:
                content = m.get("content", "")
                if isinstance(content, str):
                    assert d.scan(content) == [], f"PII leaked through: {content!r}"


@given(text=st.text(alphabet=st.characters(blacklist_categories=("Cc",)),
                    min_size=0, max_size=200))
def test_replace_mode_terminal_never_sees_pii(text):
    mw = PIIMiddleware(rules=[{
        "match": "openai:*", "action": "redact",
        "types": ["EMAIL", "US_SSN", "PHONE_NUMBER"],
        "on_detect": "replace", "scope": ["messages"],
    }])
    ctx = CallContext(provider="openai", model="gpt-4o",
                      messages=[{"role": "user", "content": text}])
    seen: list = []
    mw(ctx, next=lambda c: seen.append(c) or "ok")

    d = RegexDetector(types=["EMAIL", "US_SSN", "PHONE_NUMBER"])
    for c in seen:
        for m in c.messages:
            content = m.get("content", "")
            if isinstance(content, str):
                assert d.scan(content) == [], f"PII leaked after redact: {content!r}"
