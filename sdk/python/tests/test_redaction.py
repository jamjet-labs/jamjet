"""Unit tests for jamjet.cloud.redaction."""
from __future__ import annotations

import unittest.mock as mock

import pytest


def test_redact_email():
    from jamjet.cloud import redaction
    result = redaction.redact("contact me at user@example.com please")
    assert "user@example.com" not in result
    assert "[EMAIL_ADDRESS]" in result


def test_redact_phone():
    from jamjet.cloud import redaction
    result = redaction.redact("call me at 415-555-1234 anytime")
    assert "415-555-1234" not in result


def test_redact_clean_string():
    from jamjet.cloud import redaction
    result = redaction.redact("nothing sensitive here")
    assert result == "nothing sensitive here"


def test_redact_no_presidio_fallback(monkeypatch):
    """When Presidio is unavailable, regex fallback fires."""
    from jamjet.cloud import redaction as r
    monkeypatch.setattr(r, "_presidio_available", False)
    result = r.redact("email is a@b.com")
    assert "a@b.com" not in result
    assert "[EMAIL_ADDRESS]" in result


def test_redact_dict_nested():
    from jamjet.cloud.redaction import _redact_dict
    obj = {"messages": [{"role": "user", "content": "my email is x@y.com"}]}
    out = _redact_dict(obj)
    assert "x@y.com" not in out["messages"][0]["content"]
    assert "[EMAIL_ADDRESS]" in out["messages"][0]["content"]


def test_custom_pii_types():
    from jamjet.cloud import redaction
    result = redaction.redact(
        "email a@b.com phone 415-555-1234",
        pii_types=["EMAIL_ADDRESS"],
    )
    assert "[EMAIL_ADDRESS]" in result
    assert "415-555-1234" in result


def test_replacement_format_static():
    from jamjet.cloud import redaction as r
    original_fmt = r._config["replacement_format"]
    r._config["replacement_format"] = "[REDACTED]"
    try:
        result = r.redact("email a@b.com")
        assert "[REDACTED]" in result
        assert "[EMAIL_ADDRESS]" not in result
    finally:
        r._config["replacement_format"] = original_fmt


def test_pii_types_empty_list_disables_for_call():
    """pii_types=[] means 'disable all detectors for this call' (not 'use defaults')."""
    from jamjet.cloud import redaction
    text = "email a@b.com phone 415-555-1234"
    assert redaction.redact(text, pii_types=[]) == text


def test_configure_resets_to_defaults():
    """A second configure() call without pii_types resets to module defaults."""
    from jamjet.cloud import redaction as r
    r.configure(enabled=True, pii_types=["EMAIL_ADDRESS"])
    assert r._config["pii_types"] == ["EMAIL_ADDRESS"]
    r.configure(enabled=True)
    assert r._config["pii_types"] == r.DEFAULT_PII_TYPES
    assert r._config["replacement_format"] == "[{type}]"
    r._config["enabled"] = False  # reset


def test_scrub_event_handles_non_string_email():
    """Non-string end_user_email is left unchanged (no crash)."""
    from jamjet.cloud.events import _scrub_event
    from jamjet.cloud.redaction import _redact_dict
    out = _scrub_event(
        {"payload": {"x": "y"}, "end_user_email": 12345},
        _redact_dict,
    )
    assert out["end_user_email"] == 12345


def test_configure_redact_false_disables_in_top_level():
    """jamjet.configure(redact=False) actually turns off auto-mode."""
    import jamjet.cloud as jj
    from jamjet.cloud import redaction as r
    r.configure(enabled=True)
    assert r._config["enabled"] is True
    jj.configure(api_key="test", redact=False, auto_patch=False)
    assert r._config["enabled"] is False


def test_auto_mode_scrubs_payload():
    """Auto-mode redacts email in event payload before POST hits the network."""
    from jamjet.cloud import redaction as r
    from jamjet.cloud.config import set_config
    from jamjet.cloud.events import EventQueue

    set_config(api_key="test-key", project="test", api_url="http://x", enabled=True)
    r.configure(enabled=True)

    captured_payloads: list[dict] = []

    class FakeResp:
        status_code = 200

        def raise_for_status(self):
            pass

    def fake_post(url, json=None, headers=None, timeout=None):
        captured_payloads.append(json)
        return FakeResp()

    try:
        with mock.patch("jamjet.cloud.events.httpx.post", side_effect=fake_post):
            q = EventQueue()
            q.push({
                "trace_id": "t1",
                "span_id": "s1",
                "sequence": 0,
                "kind": "llm",
                "timestamp": "2026-04-28T00:00:00Z",
                "payload": {"content": "email me at secret@example.com"},
            })
            q._flush()

        assert captured_payloads, "No HTTP POST captured"
        events = captured_payloads[0]["events"]
        content = events[0]["payload"]["content"]
        assert "secret@example.com" not in content
        assert "[EMAIL_ADDRESS]" in content
    finally:
        r._config["enabled"] = False
