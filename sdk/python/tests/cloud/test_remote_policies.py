"""Cloud-managed policies: loaded from the routes the Cloud API has, applied
with Cloud's own decision, and never failing silently.

The old sync called GET /v1/projects/{project}/policies, a route the Cloud API
does not have, and swallowed the error, so Cloud rules never applied. Cloud is
stubbed with respx; nothing leaves the process.
"""

from __future__ import annotations

import importlib
import json
import logging
import time
from collections.abc import Iterator
from typing import Any

import httpx
import pytest
import respx

import jamjet.cloud as jc
from jamjet.cloud.policy import PolicyEvaluator

config_mod = importlib.import_module("jamjet.cloud.config")
policy_mod = importlib.import_module("jamjet.cloud.policy")

API = "https://api.jamjet.test"


class _InlineThread:
    """Stands in for threading.Thread so the background sync runs before configure() returns."""

    def __init__(self, target: Any = None, args: tuple[Any, ...] = (), **_: Any) -> None:
        self._target = target
        self._args = args

    def start(self) -> None:
        if self._target is not None:
            self._target(*self._args)


@pytest.fixture
def fresh(monkeypatch: pytest.MonkeyPatch) -> Iterator[PolicyEvaluator]:
    evaluator = PolicyEvaluator()
    monkeypatch.setattr(policy_mod, "_evaluator", evaluator)
    monkeypatch.setattr(config_mod, "_config", config_mod.CloudConfig())
    monkeypatch.setattr(jc, "init_queue", lambda **_: None)  # no flush thread
    monkeypatch.setattr(jc.threading, "Thread", _InlineThread)
    yield evaluator


def _rules(router: respx.Router, by_kind: dict[str, list[dict[str, Any]]], status: int = 200) -> respx.Route:
    def respond(request: httpx.Request) -> httpx.Response:
        kind = request.url.params.get("kind", "")
        return httpx.Response(status, json={"policies": by_kind.get(kind, [])})

    return router.get(f"{API}/v1/policies").mock(side_effect=respond)


def _evaluate(router: respx.Router, decision: str, seen: list[dict[str, Any]] | None = None) -> respx.Route:
    def respond(request: httpx.Request) -> httpx.Response:
        if seen is not None:
            seen.append(json.loads(request.content))
        return httpx.Response(200, json={"decision": decision, "dry_run": decision == "allow"})

    return router.post(f"{API}/v1/policy/evaluate").mock(side_effect=respond)


def _rule(pattern: str, kind: str = "block", environments: list[str] | None = None) -> dict[str, Any]:
    return {"kind": kind, "pattern": pattern, "environments": environments or [], "active": True}


def _configure(**kwargs: Any) -> None:
    kwargs.setdefault("remote_policy_refresh_seconds", 0)  # the timer has its own tests
    jc.configure(api_key="jj_test", project="p", api_url=API, auto_patch=False, **kwargs)


def test_background_sync_reads_the_cloud_policies_route_and_applies_its_rules(fresh: PolicyEvaluator) -> None:
    with respx.mock() as router:
        _rules(router, {"block": [_rule("wire_*")]})
        _evaluate(router, "block")
        _configure()
        decision = fresh.evaluate("wire_money")
    assert decision.blocked is True
    assert decision.pattern == "wire_*"


def test_background_sync_failure_is_logged_not_swallowed(
    fresh: PolicyEvaluator, caplog: pytest.LogCaptureFixture
) -> None:
    with respx.mock() as router, caplog.at_level(logging.WARNING, logger="jamjet.cloud"):
        _rules(router, {}, status=404)
        _configure()
    assert "could not load Cloud policies" in caplog.text
    assert fresh.evaluate("wire_money").blocked is False  # best effort: local rules only


def test_required_remote_policies_fail_closed_until_refreshed(
    fresh: PolicyEvaluator, caplog: pytest.LogCaptureFixture
) -> None:
    with respx.mock() as router, caplog.at_level(logging.ERROR, logger="jamjet.cloud"):
        route = _rules(router, {}, status=503)
        _configure(remote_policies=True)
        assert "Every tool call is blocked" in caplog.text
        assert fresh.evaluate("search").blocked is True
        assert fresh.has_rules() is True
        route.side_effect = lambda request: httpx.Response(200, json={"policies": []})
        assert jc.refresh_policies() is True
        assert fresh.evaluate("search").blocked is False


def test_required_remote_policies_need_an_api_key(fresh: PolicyEvaluator) -> None:
    with pytest.raises(ValueError):
        jc.configure(project="p", api_url=API, auto_patch=False, remote_policies=True, remote_policy_refresh_seconds=0)


def test_cloud_decides_rules_it_scopes_itself(fresh: PolicyEvaluator) -> None:
    """A dry-run or agent-scoped block rule is listed like any other; Cloud's
    evaluate answer (allow here) is what applies."""
    seen: list[dict[str, Any]] = []
    with respx.mock() as router:
        _rules(router, {"block": [_rule("wire_*")]})
        _evaluate(router, "allow", seen)
        _configure(remote_policies=True, environment="prod")
        assert fresh.evaluate("wire_money").blocked is False
        assert fresh.evaluate("wire_money").blocked is False
    assert len(seen) == 1  # cached per tool, agent and environment
    assert seen[0] == {"tool": "wire_money", "agent": "default", "environment": "prod"}


def test_tools_no_cloud_rule_could_match_are_not_sent(fresh: PolicyEvaluator) -> None:
    with respx.mock(assert_all_called=False) as router:
        _rules(router, {"block": [_rule("wire_*")]})
        evaluate = _evaluate(router, "block")
        _configure(remote_policies=True)
        assert fresh.evaluate("search").blocked is False
    assert not evaluate.called


def test_rules_for_another_environment_do_not_apply(fresh: PolicyEvaluator) -> None:
    with respx.mock(assert_all_called=False) as router:
        _rules(router, {"block": [_rule("wire_*", environments=["prod"])]})
        evaluate = _evaluate(router, "block")
        _configure(remote_policies=True, environment="dev")
        assert fresh.evaluate("wire_money").blocked is False
    assert not evaluate.called


def test_unreachable_evaluate_applies_the_matched_rule(
    fresh: PolicyEvaluator, caplog: pytest.LogCaptureFixture
) -> None:
    with respx.mock() as router, caplog.at_level(logging.WARNING, logger="jamjet.cloud"):
        _rules(router, {"require_approval": [_rule("payments.*", kind="require_approval")]})
        router.post(f"{API}/v1/policy/evaluate").mock(side_effect=httpx.ConnectError("down"))
        _configure(remote_policies=True)
        decision = fresh.evaluate("payments.refund")
    assert decision.policy_kind == "require_approval"
    assert "policy check for 'payments.refund' failed" in caplog.text


def test_a_local_allow_cannot_loosen_a_cloud_block(fresh: PolicyEvaluator) -> None:
    with respx.mock() as router:
        _rules(router, {"block": [_rule("wire_*")]})
        _evaluate(router, "block")
        _configure(remote_policies=True)
        jc.policy("allow", "*")
        assert fresh.evaluate("wire_money").blocked is True


def test_cloud_namespace_pattern_is_matched_like_cloud(fresh: PolicyEvaluator) -> None:
    """Cloud's `payments.*` also covers a tool named exactly `payments`; the
    SDK spec glob does not, so the SDK must still ask Cloud."""
    with respx.mock() as router:
        _rules(router, {"block": [_rule("payments.*")]})
        _evaluate(router, "block")
        _configure(remote_policies=True)
        assert fresh.evaluate("payments").blocked is True


def test_a_failed_reload_keeps_the_last_rules(fresh: PolicyEvaluator, caplog: pytest.LogCaptureFixture) -> None:
    with respx.mock() as router, caplog.at_level(logging.WARNING, logger="jamjet.cloud"):
        route = _rules(router, {"block": [_rule("wire_*")]})
        _evaluate(router, "block")
        _configure(remote_policies=True)
        assert fresh.evaluate("wire_money").blocked is True
        route.side_effect = lambda request: httpx.Response(503)
        assert jc.refresh_policies() is False
        assert fresh.evaluate("wire_money").blocked is True
        assert fresh.evaluate("search").blocked is False  # not fail-closed: rules had loaded
    assert "keeping the last rules loaded" in caplog.text


def test_rules_reload_on_a_timer() -> None:
    from jamjet.cloud.remote_policy import RemotePolicies

    calls = 0

    def respond(request: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(200, json={"policies": []})

    with respx.mock() as router:
        router.get(f"{API}/v1/policies").mock(side_effect=respond)
        remote = RemotePolicies("jj_test", API, None, required=True)
        remote.start_refresh(0.02)
        deadline = time.monotonic() + 5
        while calls < 4 and time.monotonic() < deadline:  # 2 kinds per load
            time.sleep(0.01)
        remote.stop()
    assert calls >= 4
