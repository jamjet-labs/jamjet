"""Tests for jamjet.cloud.cloud_pusher.CloudPusher + detect_path_mode."""

from __future__ import annotations

import time
from typing import Any

import httpx
import pytest
import respx

from jamjet.cloud.cloud_pusher import CloudPusher, detect_path_mode

SAMPLE_EVENT: dict[str, Any] = {
    "ts": "2026-05-12T00:00:00.000Z",
    "run_id": "run_a",
    "adapter": "python-sdk",
    "host": "python",
    "tool": "x.y",
    "decision": "BLOCKED",
    "executed": False,
    "schema_version": 1,
    "args": {"redacted": True},
    "args_redaction": "full",
}


@respx.mock
def test_push_returns_true_on_2xx() -> None:
    respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=200, json={})
    p = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    assert p.push(SAMPLE_EVENT) is True
    assert p.consecutive_failures == 0
    p.close()


@respx.mock
def test_push_returns_false_on_5xx() -> None:
    respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=503)
    p = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    assert p.push(SAMPLE_EVENT) is False
    assert p.consecutive_failures == 1
    p.close()


@respx.mock
def test_push_returns_false_on_4xx() -> None:
    """4xx counts as failure too — direct-push has no outbox to retry from."""
    respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=400)
    p = CloudPusher(api_base="https://api.example.com", api_key="jj_test")
    assert p.push(SAMPLE_EVENT) is False
    p.close()


@respx.mock
def test_push_request_carries_bearer_and_path_direct() -> None:
    route = respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=200, json={})
    p = CloudPusher(api_base="https://api.example.com", api_key="jj_secret_key")
    p.push(SAMPLE_EVENT)
    assert route.called
    req = route.calls[0].request
    assert req.headers.get("authorization") == "Bearer jj_secret_key"
    body = req.read().decode("utf-8")
    import json as _json

    parsed = _json.loads(body)
    assert parsed["path"] == "direct"
    assert parsed["events"][0]["run_id"] == "run_a"
    p.close()


@respx.mock
def test_trailing_slash_in_api_base_is_normalized() -> None:
    route = respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=200, json={})
    p = CloudPusher(api_base="https://api.example.com/", api_key="jj_test")
    p.push(SAMPLE_EVENT)
    assert route.called
    p.close()


@respx.mock
def test_circuit_breaker_opens_after_threshold() -> None:
    respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=503)
    p = CloudPusher(
        api_base="https://api.example.com",
        api_key="jj_test",
        circuit_breaker_threshold=3,
    )
    for _ in range(3):
        p.push(SAMPLE_EVENT)
    assert p.is_circuit_open() is True
    p.close()


@respx.mock
def test_circuit_open_skips_http_call() -> None:
    route = respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=503)
    p = CloudPusher(
        api_base="https://api.example.com",
        api_key="jj_test",
        circuit_breaker_threshold=2,
    )
    for _ in range(2):
        p.push(SAMPLE_EVENT)
    assert p.is_circuit_open() is True
    calls_before = route.call_count
    p.push(SAMPLE_EVENT)
    assert route.call_count == calls_before, "open breaker must short-circuit"
    p.close()


@respx.mock
def test_circuit_breaker_resets_after_window() -> None:
    respx.post("https://api.example.com/v1/policy-audit/events").respond(status_code=503)
    p = CloudPusher(
        api_base="https://api.example.com",
        api_key="jj_test",
        circuit_breaker_threshold=2,
        circuit_breaker_reset_seconds=0.05,
    )
    for _ in range(2):
        p.push(SAMPLE_EVENT)
    assert p.is_circuit_open() is True
    time.sleep(0.1)
    assert p.is_circuit_open() is False
    p.close()


@respx.mock
def test_successful_push_resets_failure_counter() -> None:
    route = respx.post("https://api.example.com/v1/policy-audit/events")

    def handler(request: httpx.Request) -> httpx.Response:
        # First 3 fail, then succeed.
        n = handler._n  # type: ignore[attr-defined]
        handler._n = n + 1  # type: ignore[attr-defined]
        return httpx.Response(503 if n < 3 else 200, json={})

    handler._n = 0  # type: ignore[attr-defined]
    route.side_effect = handler

    p = CloudPusher(
        api_base="https://api.example.com",
        api_key="jj_test",
        circuit_breaker_threshold=10,
    )
    for _ in range(3):
        p.push(SAMPLE_EVENT)
    assert p.consecutive_failures == 3
    p.push(SAMPLE_EVENT)
    assert p.consecutive_failures == 0
    p.close()


def test_push_never_raises_on_connect_refused() -> None:
    """No mocked server, no real server — connect refused. Must return False, never raise."""
    p = CloudPusher(
        api_base="http://127.0.0.1:1",
        api_key="jj_test",
        timeout_seconds=0.2,
    )
    assert p.push(SAMPLE_EVENT) is False
    p.close()


def _clear_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for v in (
        "JAMJET_CLOUD_TOKEN",
        "JAMJET_CLOUD_MODE",
        "VERCEL",
        "CF_PAGES",
        "AWS_LAMBDA_FUNCTION_NAME",
        "GITHUB_ACTIONS",
        "NETLIFY",
    ):
        monkeypatch.delenv(v, raising=False)


def test_detect_path_mode_no_token(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    assert detect_path_mode() == "local-only"


def test_detect_path_mode_token_but_no_serverless(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "jj_test")
    assert detect_path_mode() == "local-only"


@pytest.mark.parametrize(
    "var",
    ["VERCEL", "CF_PAGES", "AWS_LAMBDA_FUNCTION_NAME", "GITHUB_ACTIONS", "NETLIFY"],
)
def test_detect_path_mode_serverless_envs(monkeypatch: pytest.MonkeyPatch, var: str) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "jj_test")
    monkeypatch.setenv(var, "1")
    assert detect_path_mode() == "direct"


def test_detect_path_mode_explicit_direct(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "jj_test")
    monkeypatch.setenv("JAMJET_CLOUD_MODE", "direct")
    assert detect_path_mode() == "direct"


def test_detect_path_mode_explicit_daemon_wins_over_serverless(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "jj_test")
    monkeypatch.setenv("VERCEL", "1")
    monkeypatch.setenv("JAMJET_CLOUD_MODE", "daemon")
    assert detect_path_mode() == "local-only"


def test_detect_path_mode_no_token_with_serverless_is_local_only(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _clear_env(monkeypatch)
    monkeypatch.setenv("VERCEL", "1")
    assert detect_path_mode() == "local-only"
