"""Property: for any input containing or not containing PII, after the PII
middleware fires, the terminal call either NEVER runs (block path) or runs
with a CallContext that contains zero detector hits (replace path)."""

from hypothesis import given, settings
from hypothesis import strategies as st

from jamjet.cloud.exceptions import JamJetPIIBlocked
from jamjet.cloud.middleware import CallContext
from jamjet.cloud.middleware.pii import (
    PIIMiddleware,
    RegexDetector,
)

# Build each middleware ONCE at module load, not per hypothesis example: the
# detector construction loads Presidio's NLP engine, which is far too slow to
# repeat per example. The middleware holds no per-call state, so reuse is safe.
_PII_TYPES = ["EMAIL", "US_SSN", "PHONE_NUMBER"]
_BLOCK_MW = PIIMiddleware(
    rules=[
        {
            "match": "openai:*",
            "action": "redact",
            "types": _PII_TYPES,
            "on_detect": "block",
            "scope": ["messages"],
        }
    ]
)
_REPLACE_MW = PIIMiddleware(
    rules=[
        {
            "match": "openai:*",
            "action": "redact",
            "types": _PII_TYPES,
            "on_detect": "replace",
            "scope": ["messages"],
        }
    ]
)


@settings(deadline=None)  # per-example time is Presidio analysis, not the property under test
@given(
    base_text=st.text(alphabet=st.characters(blacklist_categories=("Cc",)), min_size=0, max_size=200),
)
def test_block_mode_never_passes_pii_to_terminal(base_text):
    """If the middleware doesn't raise, the terminal was called — and the
    terminal must have received content with no detector hits.
    If the middleware raises, the terminal was never called."""
    mw = _BLOCK_MW
    ctx = CallContext(provider="openai", model="gpt-4o", messages=[{"role": "user", "content": base_text}])
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


@settings(deadline=None)  # per-example time is Presidio analysis, not the property under test
@given(text=st.text(alphabet=st.characters(blacklist_categories=("Cc",)), min_size=0, max_size=200))
def test_replace_mode_terminal_never_sees_pii(text):
    mw = _REPLACE_MW
    ctx = CallContext(provider="openai", model="gpt-4o", messages=[{"role": "user", "content": text}])
    seen: list = []
    mw(ctx, next=lambda c: seen.append(c) or "ok")

    d = RegexDetector(types=["EMAIL", "US_SSN", "PHONE_NUMBER"])
    for c in seen:
        for m in c.messages:
            content = m.get("content", "")
            if isinstance(content, str):
                assert d.scan(content) == [], f"PII leaked after redact: {content!r}"
