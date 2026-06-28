"""Tests for PiiRedactionMiddleware (T3-3).

Adversarial dual coverage:
- A prompt with email + SSN + phone -> backend receives REDACTED form (PII not leaked).
- pii=False via GovernanceConfig -> backend receives the RAW prompt (passthrough).
- A clean prompt (no PII) -> backend receives the original unchanged.
- Redactor failure -> DENY (ModelDeniedError raised, backend NOT called).

Backend-receives tests use a FakeBackend that captures the request it received so
assertions can inspect what the provider would actually see.
"""

from __future__ import annotations

import pytest

from jamjet.agents.governance import normalize_governance
from jamjet.model.middleware import ModelDeniedError
from jamjet.model.pii import PiiRedactionMiddleware, _redact_messages
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse, parse_model_ref

# ---------------------------------------------------------------------------
# Shared test helpers
# ---------------------------------------------------------------------------


class CapturingBackend:
    """Fake backend that records every request it receives.

    Never calls a real provider.  Used to assert on what the provider
    would have seen after the middleware chain ran.
    """

    def __init__(self) -> None:
        self.received: list[ModelRequest] = []

    async def complete(self, request: ModelRequest) -> ModelResponse:
        self.received.append(request)
        return ModelResponse(
            message=object(),
            input_tokens=5,
            output_tokens=5,
            cost_usd=0.01,
        )


class BrokenDetector:
    """A detector that always raises (used to test the fail-closed path)."""

    def redact(self, text: str) -> str:
        raise RuntimeError("detector exploded")


def _req(content: str) -> ModelRequest:
    return ModelRequest(
        ref=parse_model_ref("anthropic/claude-opus-4-8"),
        messages=[{"role": "user", "content": content}],
    )


def _req_multipart(parts: list[dict]) -> ModelRequest:
    return ModelRequest(
        ref=parse_model_ref("anthropic/claude-opus-4-8"),
        messages=[{"role": "user", "content": parts}],
    )


# ---------------------------------------------------------------------------
# _redact_messages unit tests (the helper, no Model overhead)
# ---------------------------------------------------------------------------


class TestRedactMessages:
    def test_email_is_redacted(self) -> None:
        mw = PiiRedactionMiddleware()
        msgs = [{"role": "user", "content": "contact alice@example.com"}]
        out = _redact_messages(msgs, mw._detector)
        assert "alice@example.com" not in out[0]["content"]
        assert "[REDACTED:EMAIL]" in out[0]["content"]

    def test_ssn_is_redacted(self) -> None:
        mw = PiiRedactionMiddleware()
        msgs = [{"role": "user", "content": "ssn 123-45-6789 on file"}]
        out = _redact_messages(msgs, mw._detector)
        assert "123-45-6789" not in out[0]["content"]
        assert "[REDACTED:US_SSN]" in out[0]["content"]

    def test_phone_is_redacted(self) -> None:
        mw = PiiRedactionMiddleware()
        msgs = [{"role": "user", "content": "call 555-867-5309 now"}]
        out = _redact_messages(msgs, mw._detector)
        assert "555-867-5309" not in out[0]["content"]

    def test_clean_prompt_is_unchanged(self) -> None:
        mw = PiiRedactionMiddleware()
        original = "What is the capital of France?"
        msgs = [{"role": "user", "content": original}]
        out = _redact_messages(msgs, mw._detector)
        assert out[0]["content"] == original

    def test_multipart_content_text_parts_redacted(self) -> None:
        mw = PiiRedactionMiddleware()
        msgs = [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "ssn 123-45-6789"},
                    {"type": "image_url", "image_url": {"url": "http://example.com/img.png"}},
                    {"type": "text", "text": "email alice@example.com"},
                ],
            }
        ]
        out = _redact_messages(msgs, mw._detector)
        parts = out[0]["content"]
        assert "123-45-6789" not in parts[0]["text"]
        assert "[REDACTED:US_SSN]" in parts[0]["text"]
        assert "alice@example.com" not in parts[2]["text"]
        assert "[REDACTED:EMAIL]" in parts[2]["text"]
        # Non-text parts are untouched.
        assert parts[1]["image_url"]["url"] == "http://example.com/img.png"

    def test_caller_messages_not_mutated(self) -> None:
        """_redact_messages must not mutate the original list or dicts."""
        mw = PiiRedactionMiddleware()
        original_content = "email alice@example.com"
        msgs = [{"role": "user", "content": original_content}]
        _redact_messages(msgs, mw._detector)
        # Original dict is unchanged.
        assert msgs[0]["content"] == original_content


# ---------------------------------------------------------------------------
# PiiRedactionMiddleware.before unit tests
# ---------------------------------------------------------------------------


class TestPiiRedactionMiddlewareBefore:
    async def test_pii_in_prompt_is_redacted(self) -> None:
        mw = PiiRedactionMiddleware()
        req = _req("contact alice@example.com or ssn 123-45-6789")
        out = await mw.before(req)
        content = out.messages[0]["content"]
        assert "alice@example.com" not in content
        assert "123-45-6789" not in content
        assert "[REDACTED:EMAIL]" in content
        assert "[REDACTED:US_SSN]" in content

    async def test_clean_prompt_passes_through_unchanged(self) -> None:
        mw = PiiRedactionMiddleware()
        text = "What is the capital of France?"
        req = _req(text)
        out = await mw.before(req)
        assert out.messages[0]["content"] == text

    async def test_redactor_failure_raises_model_denied_error(self) -> None:
        """Fail-closed: a detector exception -> ModelDeniedError (redact-or-deny)."""
        mw = PiiRedactionMiddleware(detector=BrokenDetector())
        req = _req("some content")
        with pytest.raises(ModelDeniedError) as exc_info:
            await mw.before(req)
        assert exc_info.value.code == "pii_redaction_error"

    async def test_redactor_failure_raises_model_denied_subclass(self) -> None:
        """ModelDeniedError is the fail-closed family root -- verify hierarchy."""
        mw = PiiRedactionMiddleware(detector=BrokenDetector())
        with pytest.raises(ModelDeniedError):
            await mw.before(_req("text"))


# ---------------------------------------------------------------------------
# Integration via Model -- backend receives redacted text (the critical assertion)
# ---------------------------------------------------------------------------


class TestPiiRedactionViaModel:
    async def test_backend_receives_redacted_prompt(self) -> None:
        """Core T3-3 assertion: the provider NEVER sees raw PII in the prompt."""
        backend = CapturingBackend()
        model = Model(backend=backend, middleware=[PiiRedactionMiddleware()])
        await model.complete(_req("contact alice@example.com or ssn 123-45-6789 or call 555-867-5309"))
        assert backend.received, "backend must have been called"
        content = backend.received[0].messages[0]["content"]
        # PII not present.
        assert "alice@example.com" not in content
        assert "123-45-6789" not in content
        # Placeholders present.
        assert "[REDACTED:EMAIL]" in content
        assert "[REDACTED:US_SSN]" in content

    async def test_clean_prompt_backend_called_with_original(self) -> None:
        """A prompt without PII -> backend receives the original text unchanged."""
        backend = CapturingBackend()
        model = Model(backend=backend, middleware=[PiiRedactionMiddleware()])
        text = "Explain recursion in one sentence."
        await model.complete(_req(text))
        assert backend.received[0].messages[0]["content"] == text

    async def test_redactor_failure_backend_not_called(self) -> None:
        """Fail-closed: redactor error -> provider is NEVER called (adversarial dual)."""
        backend = CapturingBackend()
        model = Model(backend=backend, middleware=[PiiRedactionMiddleware(detector=BrokenDetector())])
        with pytest.raises(ModelDeniedError) as exc_info:
            await model.complete(_req("some content"))
        assert exc_info.value.code == "pii_redaction_error"
        assert backend.received == [], "backend must NOT be called on redaction failure"

    async def test_pii_off_backend_receives_raw_prompt(self) -> None:
        """pii=False via GovernanceConfig -> PiiRedactionMiddleware NOT in chain
        -> backend receives the raw (unredacted) prompt."""
        from jamjet.model.defaults import default_model_middleware

        backend = CapturingBackend()
        gov = normalize_governance(pii=False)
        model = Model(backend=backend, middleware=default_model_middleware(governance=gov))
        raw_text = "contact alice@example.com or ssn 123-45-6789"
        await model.complete(_req(raw_text))
        # Backend receives the RAW text because PII middleware was omitted.
        content = backend.received[0].messages[0]["content"]
        assert "alice@example.com" in content
        assert "123-45-6789" in content

    async def test_pii_default_on_backend_receives_redacted(self) -> None:
        """Default (no governance) -> PII is ON -> backend never sees raw PII."""
        from jamjet.model.defaults import default_model_middleware

        backend = CapturingBackend()
        model = Model(backend=backend, middleware=default_model_middleware())
        await model.complete(_req("contact alice@example.com"))
        content = backend.received[0].messages[0]["content"]
        assert "alice@example.com" not in content
        assert "[REDACTED:EMAIL]" in content


# ---------------------------------------------------------------------------
# default_model_middleware integration
# ---------------------------------------------------------------------------


class TestDefaultMiddlewarePiiIntegration:
    async def test_governance_pii_true_includes_pii_middleware(self) -> None:
        """governance.pii=True -> PiiRedactionMiddleware in the chain."""
        from jamjet.model.defaults import default_model_middleware
        from jamjet.model.pii import PiiRedactionMiddleware

        gov = normalize_governance(pii=True)
        chain = default_model_middleware(governance=gov)
        assert any(isinstance(mw, PiiRedactionMiddleware) for mw in chain)

    async def test_governance_pii_false_excludes_pii_middleware(self) -> None:
        """governance.pii=False -> PiiRedactionMiddleware NOT in the chain."""
        from jamjet.model.defaults import default_model_middleware
        from jamjet.model.pii import PiiRedactionMiddleware

        gov = normalize_governance(pii=False)
        chain = default_model_middleware(governance=gov)
        assert not any(isinstance(mw, PiiRedactionMiddleware) for mw in chain)

    async def test_pii_redaction_before_budget_check(self) -> None:
        """PII runs at index 1 (before budget at index 2) -- order documented."""
        from jamjet.agents.governance import Budget
        from jamjet.model.budget import BudgetMiddleware
        from jamjet.model.defaults import default_model_middleware
        from jamjet.model.pii import PiiRedactionMiddleware

        gov = normalize_governance(budget=Budget(cost_usd=1.0))
        chain = default_model_middleware(governance=gov)
        pii_idx = next(i for i, mw in enumerate(chain) if isinstance(mw, PiiRedactionMiddleware))
        budget_idx = next(i for i, mw in enumerate(chain) if isinstance(mw, BudgetMiddleware))
        assert pii_idx < budget_idx, "PII must run before budget check"
