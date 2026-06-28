"""PII-redaction middleware for the model seam (T3-3).

``PiiRedactionMiddleware`` rewrites ``ModelRequest.messages`` before the call
reaches any provider, replacing detected PII with typed placeholder tokens
(``[REDACTED:EMAIL]``, ``[REDACTED:US_SSN]``, etc.).

Fail-closed posture — "redact-or-deny"
---------------------------------------
If the detector raises for any reason (bug in the pattern, an unexpected
message shape, etc.) the call is DENIED by raising
``ModelDeniedError(code="pii_redaction_error")``.  The provider is never
called with unredacted text.

A prompt that contains no PII passes through unchanged.

Detector
--------
The default detector is a ``RegexDetector`` (from ``jamjet.cloud.middleware.pii``)
covering four conservative, high-signal types:

  * ``EMAIL``        -- e-mail addresses
  * ``US_SSN``       -- US social-security numbers (\\d{3}-\\d{2}-\\d{4})
  * ``CREDIT_CARD``  -- credit/debit card numbers (digit-sequence pattern)
  * ``PHONE_NUMBER`` -- North-American phone numbers

These mirror the Rust ``PiiRedactor`` types (``runtime/policy/src/redaction.rs``)
and the cloud ``RegexDetector`` matchers so redaction is consistent across all
three enforcement layers.

Presidio upgrade path
---------------------
Pass a ``CompositeDetector([RegexDetector(...), PresidioDetector(...)])`` from
``jamjet.cloud.middleware.pii`` for Presidio higher-recall NLP on top:

    from jamjet.cloud.middleware.pii import (
        CompositeDetector, PresidioDetector, RegexDetector,
    )
    detector = CompositeDetector([
        RegexDetector(types=_DEFAULT_PII_TYPES),
        PresidioDetector(types=_DEFAULT_PII_TYPES),
    ])
    mw = PiiRedactionMiddleware(detector=detector)

Output-PII redaction (the ``after`` hook) is out of scope for v1 and is
tracked as follow-up F-t3-pii-output.
"""

from __future__ import annotations

from typing import Any

from jamjet.cloud.middleware.pii import RegexDetector
from jamjet.model.middleware import BaseModelMiddleware, ModelDeniedError
from jamjet.model.types import ModelRequest

# Default PII types -- conservative, high-signal.  Mirrors the Rust redactor
# types in runtime/policy/src/redaction.rs and the cloud RegexDetector so
# the three enforcement layers use identical matchers.
_DEFAULT_PII_TYPES: list[str] = ["EMAIL", "US_SSN", "CREDIT_CARD", "PHONE_NUMBER"]


class PiiRedactionMiddleware(BaseModelMiddleware):
    """Redact PII from outbound prompt messages before the provider sees them.

    Parameters
    ----------
    detector:
        Any object implementing ``redact(text: str) -> str``.  Defaults to a
        ``RegexDetector`` covering the four default PII types.
        Inject a custom detector in tests or pass a ``CompositeDetector`` for
        Presidio-augmented recall (see module docstring for the recipe).
    pii_types:
        Override the default type list when using the built-in
        ``RegexDetector``.  Ignored when ``detector`` is provided explicitly.

    Fail-closed
    -----------
    Any exception from ``detector.redact`` raises
    ``ModelDeniedError(code="pii_redaction_error")`` -- the provider is NEVER
    called with unredacted text (redact-or-deny posture).
    """

    def __init__(
        self,
        detector: Any | None = None,
        *,
        pii_types: list[str] | None = None,
    ) -> None:
        if detector is not None:
            self._detector = detector
        else:
            types = pii_types if pii_types is not None else _DEFAULT_PII_TYPES
            self._detector = RegexDetector(types=types)

    async def before(self, request: ModelRequest) -> ModelRequest:
        """Redact PII from all message content before it reaches the provider.

        Handles both plain-string content and multi-part content lists.

        Fail-closed: any exception from the detector raises
        ``ModelDeniedError(code="pii_redaction_error")`` so the provider is
        never called with unredacted text.
        """
        try:
            request.messages = _redact_messages(request.messages, self._detector)
        except ModelDeniedError:
            raise  # already a deny -- propagate unchanged
        except Exception as exc:
            raise ModelDeniedError(
                f"PII redaction failed; call denied to prevent data leak: {exc!r}",
                code="pii_redaction_error",
            ) from exc
        return request


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _redact_messages(
    messages: list[dict[str, Any]],
    detector: Any,
) -> list[dict[str, Any]]:
    """Return a new message list with PII redacted from all text content.

    Builds a shallow copy of each message dict so the caller's list is never
    mutated.  Handles both ``"content": "string"`` and
    ``"content": [{"type": "text", "text": "..."}]`` shapes.
    """
    result: list[dict[str, Any]] = []
    for msg in messages:
        msg = dict(msg)  # shallow copy -- do not mutate caller's list
        content = msg.get("content")
        if isinstance(content, str):
            msg["content"] = detector.redact(content)
        elif isinstance(content, list):
            new_parts: list[Any] = []
            for part in content:
                if isinstance(part, dict) and isinstance(part.get("text"), str):
                    part = dict(part)  # copy before mutating
                    part["text"] = detector.redact(part["text"])
                new_parts.append(part)
            msg["content"] = new_parts
        result.append(msg)
    return result
