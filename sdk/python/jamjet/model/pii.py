"""PII-redaction middleware for the model seam (T3-3).

``PiiRedactionMiddleware`` rewrites ``ModelRequest.messages`` before the call
reaches any provider, replacing detected PII with typed placeholder tokens
(``[REDACTED:EMAIL]``, ``[REDACTED:US_SSN]``, etc.).

Fail-closed posture — "redact-or-deny"
---------------------------------------
If the detector genuinely raises while redacting a STRING it was handed
(a real redactor bug) the call is DENIED by raising
``ModelDeniedError(code="pii_redaction_error")`` so the provider is never
called with unredacted text.

Recursive, fail-closed content traversal
----------------------------------------
The redactor RECURSES through the whole message/content structure and runs the
detector on **every string it can reach**, regardless of key or nesting:
plain-string content, bare strings inside a content list, text under any key
(not only ``"text"``), and arbitrarily nested dicts/lists.  A non-dict message
(a provider SDK message object or a test mock) has its textual ``content``
surface redacted on a copy.  Genuinely non-textual leaves — ``bytes``, numbers,
``None``, an image-URL with no PII — pass through.

If a reachable value is an opaque object the redactor cannot traverse but which
may hold string data, the call is DENIED (``ModelDeniedError`` with code
``pii_unredactable_content``) rather than forwarded — fail-CLOSED: any reachable
string is redacted, or the call is denied; nothing unredacted reaches the
provider.

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

import copy
from collections.abc import Mapping
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
    Every reachable string is redacted.  A genuine exception from
    ``detector.redact`` on string input raises
    ``ModelDeniedError(code="pii_redaction_error")``; an opaque value the
    redactor cannot traverse (but which may hold strings) raises
    ``ModelDeniedError(code="pii_unredactable_content")``.  Either way the
    provider is NEVER called with unredacted text (redact-or-deny posture).
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

        Recurses through every message and content block and runs the detector
        on every reachable string (regardless of key or nesting).  Fail-closed:
        a detector error on string input raises
        ``ModelDeniedError(code="pii_redaction_error")``; an opaque value the
        redactor cannot traverse raises
        ``ModelDeniedError(code="pii_unredactable_content")`` — so the provider
        is never called with unredacted text.
        """
        request.messages = _redact_messages(request.messages, self._detector)
        return request


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _redact_text(text: str, detector: Any) -> str:
    """Redact a single string, denying only on a genuine detector failure.

    This is the *one* place the fail-closed deny path lives: an exception from
    ``detector.redact`` on real string input becomes a
    ``ModelDeniedError(code="pii_redaction_error")`` so the provider is never
    called with text the detector could not clear.  Callers must only pass
    strings here — unexpected message shapes are filtered out upstream so they
    never reach (and never trip) this deny path.
    """
    try:
        return detector.redact(text)
    except ModelDeniedError:
        raise  # already a deny -- propagate unchanged
    except Exception as exc:
        raise ModelDeniedError(
            f"PII redaction failed; call denied to prevent data leak: {exc!r}",
            code="pii_redaction_error",
        ) from exc


class _UnredactableError(Exception):
    """Internal sentinel: an opaque value the redactor cannot traverse but which
    may hold string data.  Caught at the message boundary and turned into a
    fail-closed ``ModelDeniedError`` (never propagated to callers)."""


def _redact_value(value: Any, detector: Any) -> Any:
    """Recursively redact every string reachable inside ``value``.

    ``str`` -> redacted; ``Mapping`` -> values recursed (keys are field names,
    left as-is); ``list`` / ``tuple`` -> elements recursed; ``bytes``-like and
    non-string scalars (``bool`` / ``int`` / ``float`` / ``None``) pass through
    as genuinely non-textual.  Any other opaque object raises
    :class:`_UnredactableError` so the caller fails CLOSED rather than forwarding a
    string it could not reach.  Builds new containers so the caller's structures
    are never mutated.
    """
    if isinstance(value, str):
        return _redact_text(value, detector)
    # bool is an int subclass; both (and float / None) are non-textual scalars.
    if value is None or isinstance(value, (bool, int, float)):
        return value
    if isinstance(value, (bytes, bytearray, memoryview)):
        return value
    if isinstance(value, Mapping):
        return {k: _redact_value(v, detector) for k, v in value.items()}
    if isinstance(value, list):
        return [_redact_value(v, detector) for v in value]
    if isinstance(value, tuple):
        return tuple(_redact_value(v, detector) for v in value)
    # An opaque object that is not provably non-textual -- cannot guarantee no
    # string slips through, so fail CLOSED.
    raise _UnredactableError(type(value).__name__)


def _redact_message(msg: Any, detector: Any) -> Any:
    """Redact one message, returning a redacted copy (never mutating ``msg``).

    A mapping message is recursed in full.  A provider/mock message OBJECT (the
    assistant message the in-process loop appends back into the running list)
    has its textual ``content`` surface redacted on a shallow copy.  An object
    with no recognizable textual surface fails CLOSED via :class:`_UnredactableError`.
    """
    if isinstance(msg, Mapping):
        return _redact_value(msg, detector)

    # Message-like object (e.g. a provider SDK message or a test mock): redact
    # its ``content``.  When there is no PII the original is returned untouched
    # (no copy, no mutation); only a genuine change forces a redacted copy.
    if hasattr(msg, "content"):
        content = msg.content
        if content is None:
            return msg
        redacted = _redact_value(content, detector)  # may raise _UnredactableError
        if redacted == content:
            return msg
        new = copy.copy(msg)
        try:
            new.content = redacted
        except Exception as exc:  # noqa: BLE001 - read-only/frozen -> cannot redact -> deny
            raise _UnredactableError(type(msg).__name__) from exc
        return new

    # Opaque, non-mapping message with no content surface -- fail CLOSED.
    raise _UnredactableError(type(msg).__name__)


def _redact_messages(
    messages: list[dict[str, Any]],
    detector: Any,
) -> list[dict[str, Any]]:
    """Return a new message list with PII redacted from every reachable string.

    Recurses through each message (and arbitrarily nested content) and redacts
    all strings.  Fail-closed: a detector error on string input raises
    ``ModelDeniedError(code="pii_redaction_error")`` (in :func:`_redact_text`);
    an opaque value the redactor cannot traverse raises
    ``ModelDeniedError(code="pii_unredactable_content")``.  The caller's list and
    dicts are never mutated.
    """
    # Degenerate / unexpected top-level shape -- nothing to iterate, pass through.
    if not isinstance(messages, list):
        return messages

    result: list[Any] = []
    for msg in messages:
        try:
            result.append(_redact_message(msg, detector))
        except _UnredactableError as exc:
            raise ModelDeniedError(
                f"PII redaction cannot traverse message content of type {exc}; call "
                "denied to prevent sending unredacted data to the provider.",
                code="pii_unredactable_content",
            ) from exc
    return result
