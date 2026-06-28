"""Tests for the runtime-target + auth resolver (Track 7a-1).

``resolve_runtime_target`` maps a friendly runtime name (or a bare URL) to a
``RuntimeTarget`` carrying the engine URL, an optional bearer token, whether
JamJet Cloud governance should be layered on, and a stable name. The three
named legs (local / self-host / cloud) are the SAME ``jamjet-server`` engine,
distinguished only by URL + token; "cloud" additionally records that Cloud
span-push governance is wired (it is never load-bearing).
"""

from __future__ import annotations

import pytest

from jamjet.deploy import RuntimeTarget, resolve_runtime_target

_LOCAL_URL = "http://127.0.0.1:7700"


def _clear_runtime_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for var in (
        "JAMJET_RUNTIME_URL",
        "JAMJET_RUNTIME_TOKEN",
        "JAMJET_CLOUD_RUNTIME_URL",
        "JAMJET_CLOUD_TOKEN",
        "JAMJET_API_KEY",
    ):
        monkeypatch.delenv(var, raising=False)


# ── local (the dev default) ───────────────────────────────────────────────────


def test_none_resolves_to_local(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    target = resolve_runtime_target(None)
    assert isinstance(target, RuntimeTarget)
    assert target.url == _LOCAL_URL
    assert target.token is None
    assert target.cloud_governance is False
    assert target.name == "local"


def test_local_string_resolves_to_local(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    target = resolve_runtime_target("local")
    assert target.url == _LOCAL_URL
    assert target.token is None
    assert target.cloud_governance is False
    assert target.name == "local"


# ── self-host ─────────────────────────────────────────────────────────────────


def test_self_host_reads_env_url_and_token(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    monkeypatch.setenv("JAMJET_RUNTIME_TOKEN", "tok-self")
    target = resolve_runtime_target("self-host")
    assert target.url == "https://engine.internal:8080"
    assert target.token == "tok-self"
    assert target.cloud_governance is False
    assert target.name == "self-host"


def test_self_host_token_optional(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    monkeypatch.setenv("JAMJET_RUNTIME_URL", "https://engine.internal:8080")
    target = resolve_runtime_target("self-host")
    assert target.url == "https://engine.internal:8080"
    assert target.token is None


def test_self_host_unset_url_raises_clear_error(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    with pytest.raises(ValueError, match="JAMJET_RUNTIME_URL"):
        resolve_runtime_target("self-host")


# ── cloud (hosted engine + Cloud governance) ──────────────────────────────────


def test_cloud_reads_env_url_and_enables_governance(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_RUNTIME_URL", "https://my-engine.fly.dev")
    monkeypatch.setenv("JAMJET_CLOUD_TOKEN", "tok-cloud")
    target = resolve_runtime_target("cloud")
    assert target.url == "https://my-engine.fly.dev"
    assert target.token == "tok-cloud"
    assert target.cloud_governance is True
    assert target.name == "cloud"


def test_cloud_token_falls_back_to_api_key(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    monkeypatch.setenv("JAMJET_CLOUD_RUNTIME_URL", "https://my-engine.fly.dev")
    monkeypatch.setenv("JAMJET_API_KEY", "jj_fallback")
    target = resolve_runtime_target("cloud")
    assert target.token == "jj_fallback"
    assert target.cloud_governance is True


def test_cloud_unset_url_raises_honest_error(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    # Even with an API key present, the hosted ENGINE url is required — the
    # error must name JAMJET_CLOUD_RUNTIME_URL and must NOT point at the span API.
    monkeypatch.setenv("JAMJET_API_KEY", "jj_key")
    with pytest.raises(ValueError) as exc:
        resolve_runtime_target("cloud")
    msg = str(exc.value)
    assert "JAMJET_CLOUD_RUNTIME_URL" in msg
    assert "api.jamjet.dev" not in msg  # the span API is NOT an execution engine


# ── a bare URL passthrough ────────────────────────────────────────────────────


def test_bare_http_url_passes_through(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    target = resolve_runtime_target("https://my-engine.example.com")
    assert target.url == "https://my-engine.example.com"
    assert target.token is None
    assert target.cloud_governance is False
    assert target.name == "https://my-engine.example.com"


def test_bare_http_url_lowercase_scheme(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    target = resolve_runtime_target("http://localhost:9999")
    assert target.url == "http://localhost:9999"
    assert target.cloud_governance is False


def test_unknown_runtime_name_raises(monkeypatch: pytest.MonkeyPatch) -> None:
    _clear_runtime_env(monkeypatch)
    with pytest.raises(ValueError, match="unknown runtime"):
        resolve_runtime_target("banana")
