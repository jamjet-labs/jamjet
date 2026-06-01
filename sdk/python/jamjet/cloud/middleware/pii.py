"""Phase 1 PII middleware — detectors + middleware wrapper.

Reuses the same six PII types the existing ingress-side
`jamjet.cloud.redaction` module already recognises. The detector layer is
pluggable: RegexDetector is always available; PresidioDetector wraps the
optional `presidio-analyzer` dependency (pip install jamjet[pii])."""

from __future__ import annotations

import fnmatch
import logging
import re
from abc import ABC, abstractmethod
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from jamjet.cloud.middleware import CallContext, MiddlewareOutcome

_logger = logging.getLogger("jamjet.cloud.middleware.pii")


# Patterns intentionally mirror jamjet.cloud.redaction.DEFAULT_PII_TYPES so a
# value redacted server-side is also redacted pre-LLM with the same matcher.
_REGEX_PATTERNS: dict[str, re.Pattern[str]] = {
    "EMAIL": re.compile(r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z0-9-.]+"),
    "US_SSN": re.compile(r"\b\d{3}-\d{2}-\d{4}\b"),
    "CREDIT_CARD": re.compile(r"\b(?:\d[ -]?){13,19}\b"),
    "PHONE_NUMBER": re.compile(r"\b(?:\+?1[ -]?)?\(?\d{3}\)?[ -.]?\d{3}[ -.]?\d{4}\b"),
    "IP_ADDRESS": re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    "IBAN_CODE": re.compile(r"\b[A-Z]{2}\d{2}[A-Z0-9]{1,30}\b"),
}


@dataclass
class PIIDetection:
    type: str  # one of _REGEX_PATTERNS.keys()
    value: str  # the matched substring (kept in-memory only, never logged)
    start: int  # offset in the source string
    end: int


class PIIDetector(ABC):
    """All detector implementations are configured at chain-build time with
    a list of types to scan for. They scan strings and return zero or more
    detections; redact() returns a copy of the input with detections
    substituted by `[REDACTED:<TYPE>]` tokens."""

    @abstractmethod
    def scan(self, text: str) -> list[PIIDetection]: ...

    def redact(self, text: str) -> str:
        detections = self.scan(text)
        if not detections:
            return text
        # Process right-to-left so offsets stay valid as we substitute.
        out = text
        for d in sorted(detections, key=lambda x: x.start, reverse=True):
            out = out[: d.start] + f"[REDACTED:{d.type}]" + out[d.end :]
        return out


class RegexDetector(PIIDetector):
    """Zero-dependency, always-available detector. Each requested type maps
    to a precompiled regex; scan() iterates them in declared-types order so
    overlapping matches (rare) are deterministic."""

    def __init__(self, types: list[str]) -> None:
        unknown = [t for t in types if t not in _REGEX_PATTERNS]
        if unknown:
            raise ValueError(f"unknown PII types: {unknown}")
        self._types = list(types)

    def scan(self, text: str) -> list[PIIDetection]:
        if not text:
            return []
        detections: list[PIIDetection] = []
        for t in self._types:
            for m in _REGEX_PATTERNS[t].finditer(text):
                detections.append(PIIDetection(type=t, value=m.group(0), start=m.start(), end=m.end()))
        return detections


_PRESIDIO_TYPE_MAP = {
    "EMAIL": "EMAIL_ADDRESS",
    "US_SSN": "US_SSN",
    "CREDIT_CARD": "CREDIT_CARD",
    "PHONE_NUMBER": "PHONE_NUMBER",
    "IP_ADDRESS": "IP_ADDRESS",
    "IBAN_CODE": "IBAN_CODE",
}


class PresidioDetector(PIIDetector):
    """Higher-precision detector backed by Microsoft Presidio. Activated
    automatically by the PII middleware when `presidio-analyzer` is on the
    import path. Falls back to RegexDetector when not installed.

    Install via: pip install jamjet[pii]"""

    def __init__(self, types: list[str]) -> None:
        try:
            from presidio_analyzer import AnalyzerEngine
        except ImportError as e:
            raise ImportError(
                "PresidioDetector requires `presidio-analyzer`. Install via: pip install jamjet[pii]"
            ) from e
        unknown = [t for t in types if t not in _PRESIDIO_TYPE_MAP]
        if unknown:
            raise ValueError(f"unknown PII types: {unknown}")
        self._types = list(types)
        self._engine = AnalyzerEngine()

    def scan(self, text: str) -> list[PIIDetection]:
        if not text:
            return []
        presidio_types = [_PRESIDIO_TYPE_MAP[t] for t in self._types]
        results = self._engine.analyze(text=text, entities=presidio_types, language="en")
        out: list[PIIDetection] = []
        reverse_map = {v: k for k, v in _PRESIDIO_TYPE_MAP.items()}
        for r in results:
            our_type = reverse_map.get(r.entity_type, r.entity_type)
            out.append(
                PIIDetection(
                    type=our_type,
                    value=text[r.start : r.end],
                    start=r.start,
                    end=r.end,
                )
            )
        return out


class CompositeDetector(PIIDetector):
    """Runs every sub-detector and returns the UNION of their detections,
    de-overlapped so redact() substitutes each span exactly once.

    RegexDetector is always included as a floor. Presidio's NLP model has
    higher recall for context-dependent entities (names, locations) but can
    miss structured patterns the regex layer catches deterministically — e.g.
    Presidio's `en_core_web_lg` returns nothing for the bare SSN
    `123-45-6789`, which the regex US_SSN pattern matches. Unioning keeps the
    deterministic floor so installing the higher-precision detector can never
    regress detection below regex."""

    def __init__(self, detectors: list[PIIDetector]) -> None:
        if not detectors:
            raise ValueError("CompositeDetector requires at least one detector")
        self._detectors = list(detectors)

    def scan(self, text: str) -> list[PIIDetection]:
        merged: list[PIIDetection] = []
        for det in self._detectors:
            merged.extend(det.scan(text))
        # De-overlap: sort by start, longest span first, then greedily keep
        # detections that don't overlap one already kept. This leaves redact()
        # operating on disjoint spans (it substitutes right-to-left).
        merged.sort(key=lambda x: (x.start, -(x.end - x.start)))
        kept: list[PIIDetection] = []
        last_end = -1
        for d in merged:
            if d.start >= last_end:
                kept.append(d)
                last_end = d.end
        return kept


def _build_detector(types: list[str]) -> PIIDetector:
    """RegexDetector is always the floor; Presidio is unioned on top for higher
    recall when it is installed and initialises. If Presidio is absent or fails
    to initialise (not installed, or its spaCy model cannot load), fall back to
    regex only — PII must never be silently let through."""
    regex = RegexDetector(types=types)
    try:
        presidio = PresidioDetector(types=types)
    except Exception as e:  # ImportError (not installed) or model-load failure
        _logger.info("presidio detector unavailable (%r) — using regex floor only", e)
        return regex
    return CompositeDetector([regex, presidio])


class PIIMiddleware:
    """Implements the PreCallMiddleware Protocol. Constructed once at
    chain-build time with the policy's `redact` rules; per call, finds the
    first rule whose `match` matches `ctx.identifier` and applies its
    `on_detect` action."""

    def __init__(self, rules: list[dict[str, Any]]) -> None:
        if not rules:
            raise ValueError("PIIMiddleware requires at least one redact rule")
        # Pre-build one detector per rule. Detectors are cheap to hold and
        # the type-list-per-rule is fixed.
        self._compiled = [(r, _build_detector(r["types"])) for r in rules]

    def __call__(self, ctx: CallContext, next: Callable[[CallContext], Any]) -> Any:
        rule, detector = self._match_rule(ctx)
        if rule is None:
            return next(ctx)
        # _match_rule returns rule and detector together (or both None).
        assert detector is not None

        scope: list[str] = rule.get("scope", ["messages", "tools"])
        on_detect: str = rule.get("on_detect", "block")

        try:
            detections = self._scan_ctx(ctx, detector, scope)
        except Exception as e:  # pragma: no cover — fail-open per spec
            _logger.warning("pii detector error: %r — failing open", e)
            ctx.middleware_fired.append("pii.redact")
            ctx.middleware_outcome = MiddlewareOutcome.DETECTOR_ERROR
            return next(ctx)

        if not detections:
            return next(ctx)

        if on_detect == "block":
            from jamjet.cloud.exceptions import JamJetPIIBlocked

            types_detected = sorted({d.type for d in detections})
            # Sanitized evidence: types + count only; PII VALUE NEVER LOGGED.
            ctx.middleware_fired.append("pii.redact")
            ctx.middleware_outcome = MiddlewareOutcome.BLOCKED
            ctx.middleware_evidence = {
                "types": types_detected,
                "count": len(detections),
                "action": "blocked",
            }
            raise JamJetPIIBlocked(
                rule_pattern=rule["match"],
                types_detected=types_detected,
            )

        if on_detect == "replace":
            types_detected = sorted({d.type for d in detections})
            self._redact_ctx(ctx, detector, scope)
            ctx.middleware_fired.append("pii.redact")
            ctx.middleware_outcome = MiddlewareOutcome.PASSTHROUGH
            ctx.middleware_evidence = {
                "types": types_detected,
                "count": len(detections),
                "action": "replaced",
            }
            return next(ctx)

        # Unknown on_detect (shouldn't reach here — validator catches it) -> fail-open.
        _logger.warning("pii rule has unknown on_detect: %r — failing open", on_detect)
        return next(ctx)

    def _match_rule(self, ctx: CallContext) -> tuple[dict[str, Any] | None, PIIDetector | None]:
        for rule, detector in self._compiled:
            if fnmatch.fnmatch(ctx.identifier, rule["match"]):
                return rule, detector
        return None, None

    def _scan_ctx(self, ctx: CallContext, detector: PIIDetector, scope: list[str]) -> list[PIIDetection]:
        detections: list[PIIDetection] = []
        if "messages" in scope:
            for msg in ctx.messages:
                content = msg.get("content")
                if isinstance(content, str):
                    detections.extend(detector.scan(content))
                elif isinstance(content, list):
                    for part in content:
                        if isinstance(part, dict) and isinstance(part.get("text"), str):
                            detections.extend(detector.scan(part["text"]))
        if "tools" in scope:
            for tool in ctx.tools:
                fn = tool.get("function", tool) if isinstance(tool, dict) else {}
                if isinstance(fn.get("description"), str):
                    detections.extend(detector.scan(fn["description"]))
                params = fn.get("parameters")
                if isinstance(params, dict):
                    # Scan parameter descriptions only (the schema itself is structure, not user content)
                    for prop in (params.get("properties") or {}).values():
                        if isinstance(prop, dict) and isinstance(prop.get("description"), str):
                            detections.extend(detector.scan(prop["description"]))
        # System prompt deliberately NOT scanned — per spec section 4.
        return detections

    def _redact_ctx(self, ctx: CallContext, detector: PIIDetector, scope: list[str]) -> None:
        if "messages" in scope:
            for msg in ctx.messages:
                content = msg.get("content")
                if isinstance(content, str):
                    msg["content"] = detector.redact(content)
                elif isinstance(content, list):
                    for part in content:
                        if isinstance(part, dict) and isinstance(part.get("text"), str):
                            part["text"] = detector.redact(part["text"])
        if "tools" in scope:
            for tool in ctx.tools:
                fn = tool.get("function", tool) if isinstance(tool, dict) else {}
                if isinstance(fn.get("description"), str):
                    fn["description"] = detector.redact(fn["description"])
