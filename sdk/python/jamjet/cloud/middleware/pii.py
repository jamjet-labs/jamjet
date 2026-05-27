"""Phase 1 PII middleware — detectors + middleware wrapper.

Reuses the same six PII types the existing ingress-side
`jamjet.cloud.redaction` module already recognises. The detector layer is
pluggable: RegexDetector is always available; PresidioDetector wraps the
optional `presidio-analyzer` dependency (pip install jamjet[pii])."""
from __future__ import annotations
import re
from abc import ABC, abstractmethod
from dataclasses import dataclass


# Patterns intentionally mirror jamjet.cloud.redaction.DEFAULT_PII_TYPES so a
# value redacted server-side is also redacted pre-LLM with the same matcher.
_REGEX_PATTERNS: dict[str, re.Pattern[str]] = {
    "EMAIL":         re.compile(r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z0-9-.]+"),
    "US_SSN":        re.compile(r"\b\d{3}-\d{2}-\d{4}\b"),
    "CREDIT_CARD":   re.compile(r"\b(?:\d[ -]?){13,19}\b"),
    "PHONE_NUMBER":  re.compile(r"\b(?:\+?1[ -]?)?\(?\d{3}\)?[ -.]?\d{3}[ -.]?\d{4}\b"),
    "IP_ADDRESS":    re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    "IBAN_CODE":     re.compile(r"\b[A-Z]{2}\d{2}[A-Z0-9]{1,30}\b"),
}


@dataclass
class PIIDetection:
    type: str           # one of _REGEX_PATTERNS.keys()
    value: str          # the matched substring (kept in-memory only, never logged)
    start: int          # offset in the source string
    end: int


class PIIDetector(ABC):
    """All detector implementations are configured at chain-build time with
    a list of types to scan for. They scan strings and return zero or more
    detections; redact() returns a copy of the input with detections
    substituted by `[REDACTED:<TYPE>]` tokens."""

    @abstractmethod
    def scan(self, text: str) -> list[PIIDetection]:
        ...

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
                detections.append(PIIDetection(type=t, value=m.group(0),
                                               start=m.start(), end=m.end()))
        return detections


_PRESIDIO_TYPE_MAP = {
    "EMAIL":        "EMAIL_ADDRESS",
    "US_SSN":       "US_SSN",
    "CREDIT_CARD":  "CREDIT_CARD",
    "PHONE_NUMBER": "PHONE_NUMBER",
    "IP_ADDRESS":   "IP_ADDRESS",
    "IBAN_CODE":    "IBAN_CODE",
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
                "PresidioDetector requires `presidio-analyzer`. Install via: "
                "pip install jamjet[pii]"
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
            out.append(PIIDetection(
                type=our_type,
                value=text[r.start : r.end],
                start=r.start,
                end=r.end,
            ))
        return out
