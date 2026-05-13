"""Tests for jamjet.cloud.trace_context."""

from __future__ import annotations

import pytest

from jamjet.cloud.trace_context import (
    Traceparent,
    parse_traceparent,
    read_traceparent,
)

VALID = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
VALID_TID = "0af7651916cd43dd8448eb211c80319c"


def test_parse_valid() -> None:
    r = parse_traceparent(VALID)
    assert r == Traceparent(
        version="00",
        trace_id=VALID_TID,
        parent_id="b7ad6b7169203331",
        flags="01",
    )


def test_parse_malformed() -> None:
    assert parse_traceparent("garbage") is None
    assert parse_traceparent("") is None
    assert parse_traceparent(None) is None
    assert parse_traceparent(123) is None  # type: ignore[arg-type]


def test_parse_rejects_reserved_version() -> None:
    assert parse_traceparent(f"ff-{VALID_TID}-b7ad6b7169203331-01") is None


def test_parse_rejects_all_zero_trace_id() -> None:
    assert parse_traceparent("00-" + "0" * 32 + "-b7ad6b7169203331-01") is None


def test_parse_rejects_all_zero_parent_id() -> None:
    assert parse_traceparent(f"00-{VALID_TID}-" + "0" * 16 + "-01") is None


def test_parse_normalizes_hex_to_lowercase() -> None:
    upper = "00-0AF7651916CD43DD8448EB211C80319C-B7AD6B7169203331-01"
    r = parse_traceparent(upper)
    assert r is not None
    assert r.trace_id == VALID_TID
    assert r.parent_id == "b7ad6b7169203331"


def test_parse_tolerates_whitespace() -> None:
    assert parse_traceparent(f"  {VALID}  ") is not None


def test_read_from_headers_lowercase() -> None:
    r = read_traceparent({"traceparent": VALID})
    assert r is not None
    assert r.trace_id == VALID_TID


def test_read_from_headers_titlecase() -> None:
    r = read_traceparent({"Traceparent": VALID})
    assert r is not None
    assert r.trace_id == VALID_TID


def test_read_from_headers_list_value() -> None:
    r = read_traceparent({"traceparent": [VALID]})
    assert r is not None
    assert r.trace_id == VALID_TID


def test_read_from_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("OTEL_TRACE_ID", raising=False)
    monkeypatch.setenv("OTEL_TRACE_ID", VALID_TID)
    r = read_traceparent()
    assert r is not None
    assert r.trace_id == VALID_TID
    assert r.parent_id == "0" * 16


def test_read_returns_none_when_no_source(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("OTEL_TRACE_ID", raising=False)
    assert read_traceparent() is None


def test_read_ignores_malformed_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OTEL_TRACE_ID", "too-short")
    assert read_traceparent() is None


def test_header_source_wins_over_env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("OTEL_TRACE_ID", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    other_tid = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    r = read_traceparent({"traceparent": f"00-{other_tid}-cccccccccccccccc-01"})
    assert r is not None
    assert r.trace_id == other_tid
