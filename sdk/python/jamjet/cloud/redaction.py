"""PII redaction for JamJet Cloud SDK.

Uses Presidio (presidio-analyzer + spacy en_core_web_lg) when installed.
Falls back to compiled regex patterns otherwise.
"""
from __future__ import annotations

import re
from typing import Any

try:
    from presidio_analyzer import AnalyzerEngine
    from presidio_anonymizer import AnonymizerEngine
    from presidio_anonymizer.entities import OperatorConfig

    _presidio_available = True
    _analyzer: Any = None
    _anonymizer: Any = None
except ImportError:
    _presidio_available = False
    _analyzer = None
    _anonymizer = None


_REGEX_PATTERNS: dict[str, re.Pattern[str]] = {
    "EMAIL_ADDRESS": re.compile(
        r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}"
    ),
    "CREDIT_CARD": re.compile(r"\b(?:\d[\s\-]?){13,15}\d\b"),
    "US_SSN": re.compile(r"\b\d{3}[-\s]?\d{2}[-\s]?\d{4}\b"),
    "PHONE_NUMBER": re.compile(
        r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b"
    ),
    "IP_ADDRESS": re.compile(
        r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b"
    ),
    "IBAN_CODE": re.compile(r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b"),
}

DEFAULT_PII_TYPES = list(_REGEX_PATTERNS.keys())


_config: dict[str, Any] = {
    "enabled": False,
    "pii_types": DEFAULT_PII_TYPES,
    "replacement_format": "[{type}]",
}


def _setup_presidio() -> None:
    global _analyzer, _anonymizer
    if _presidio_available and _analyzer is None:
        _analyzer = AnalyzerEngine()
        _anonymizer = AnonymizerEngine()


def _make_replacement(pii_type: str) -> str:
    return _config["replacement_format"].replace("{type}", pii_type)


def _redact_regex(text: str, pii_types: list[str]) -> str:
    result = text
    for pii_type in pii_types:
        pattern = _REGEX_PATTERNS.get(pii_type)
        if pattern:
            result = pattern.sub(_make_replacement(pii_type), result)
    return result


def _redact_presidio(text: str, pii_types: list[str]) -> str:
    _setup_presidio()
    if _analyzer is None or _anonymizer is None:
        return _redact_regex(text, pii_types)
    results = _analyzer.analyze(text=text, entities=pii_types, language="en")
    if not results:
        return text
    operators = {
        r.entity_type: OperatorConfig(
            "replace", {"new_value": _make_replacement(r.entity_type)}
        )
        for r in results
    }
    return _anonymizer.anonymize(
        text=text, analyzer_results=results, operators=operators
    ).text


def redact(text: str, *, pii_types: list[str] | None = None) -> str:
    """Redact PII from text. Uses Presidio if available, else regex."""
    types = pii_types or _config["pii_types"]
    if _presidio_available:
        return _redact_presidio(text, types)
    return _redact_regex(text, types)


def _redact_dict(obj: Any) -> Any:
    """Recursively redact PII from all string values in a dict/list."""
    if isinstance(obj, str):
        return redact(obj)
    if isinstance(obj, dict):
        return {k: _redact_dict(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [_redact_dict(item) for item in obj]
    return obj


def configure(
    *,
    enabled: bool = True,
    pii_types: list[str] | None = None,
    replacement_format: str | None = None,
) -> None:
    """Configure auto-mode. Called by jamjet.configure(redact=True)."""
    _config["enabled"] = enabled
    if pii_types is not None:
        _config["pii_types"] = pii_types
    if replacement_format is not None:
        _config["replacement_format"] = replacement_format
