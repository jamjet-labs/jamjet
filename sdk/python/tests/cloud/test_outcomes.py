"""Tests for jamjet.cloud.outcomes.record_outcome."""

from __future__ import annotations

import json
from typing import Any

import httpx
import pytest
import respx

from jamjet.cloud.outcomes import VALID_OUTCOMES, record_outcome

API_URL = "https://api.example.com"
API_KEY = "jj_test_key"
OUTCOMES_URL = f"{API_URL}/v1/outcomes"


# ---------------------------------------------------------------------------
# Happy-path HTTP behaviour
# ---------------------------------------------------------------------------


@respx.mock
def test_posts_to_correct_url_with_bearer_auth() -> None:
    route = respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})

    record_outcome(API_KEY, API_URL, trace_id="trace_abc", outcome="success")

    assert route.called
    req: httpx.Request = route.calls[0].request
    assert req.headers["authorization"] == f"Bearer {API_KEY}"
    assert req.url.path == "/v1/outcomes"


@respx.mock
def test_request_body_minimal() -> None:
    """Minimal call: only trace_id + outcome in body."""
    route = respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})

    record_outcome(API_KEY, API_URL, trace_id="trace_abc", outcome="success")

    body: dict[str, Any] = json.loads(route.calls[0].request.content)
    assert body == {"trace_id": "trace_abc", "outcome": "success"}


@respx.mock
def test_request_body_with_score_and_metadata() -> None:
    route = respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})

    record_outcome(
        API_KEY,
        API_URL,
        trace_id="trace_xyz",
        outcome="failure",
        score=0.42,
        metadata={"reason": "timeout", "retries": 3},
    )

    body: dict[str, Any] = json.loads(route.calls[0].request.content)
    assert body["trace_id"] == "trace_xyz"
    assert body["outcome"] == "failure"
    assert body["score"] == pytest.approx(0.42)
    assert body["metadata"] == {"reason": "timeout", "retries": 3}


@respx.mock
@pytest.mark.parametrize("outcome", sorted(VALID_OUTCOMES))
def test_all_valid_outcomes_are_accepted(outcome: str) -> None:
    respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})
    record_outcome(API_KEY, API_URL, trace_id="t1", outcome=outcome)  # must not raise


@respx.mock
def test_score_boundary_zero_and_one_are_accepted() -> None:
    respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})
    record_outcome(API_KEY, API_URL, trace_id="t1", outcome="success", score=0)
    respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})
    record_outcome(API_KEY, API_URL, trace_id="t2", outcome="success", score=1)


@respx.mock
def test_raises_http_status_error_on_non_2xx() -> None:
    respx.post(OUTCOMES_URL).respond(status_code=422)

    with pytest.raises(httpx.HTTPStatusError):
        record_outcome(API_KEY, API_URL, trace_id="t1", outcome="success")


# ---------------------------------------------------------------------------
# Input validation
# ---------------------------------------------------------------------------


def test_raises_value_error_for_invalid_outcome() -> None:
    with pytest.raises(ValueError, match="Invalid outcome"):
        record_outcome(API_KEY, API_URL, trace_id="t1", outcome="wrong")


def test_raises_value_error_for_score_below_zero() -> None:
    with pytest.raises(ValueError, match="score must be a number between 0 and 1"):
        record_outcome(API_KEY, API_URL, trace_id="t1", outcome="success", score=-0.1)


def test_raises_value_error_for_score_above_one() -> None:
    with pytest.raises(ValueError, match="score must be a number between 0 and 1"):
        record_outcome(API_KEY, API_URL, trace_id="t1", outcome="success", score=1.01)


# ---------------------------------------------------------------------------
# __init__.py surface: the configure() → record_outcome() wrapper
# ---------------------------------------------------------------------------


@respx.mock
def test_public_wrapper_posts_with_configured_key(monkeypatch: pytest.MonkeyPatch) -> None:
    """jamjet.cloud.record_outcome should pick up api_key / api_url from config."""
    from jamjet.cloud import record_outcome as pub_record_outcome
    from jamjet.cloud.config import set_config

    set_config(api_key="jj_from_config", api_url=API_URL)

    route = respx.post(OUTCOMES_URL).respond(status_code=200, json={"recorded": True})
    pub_record_outcome(trace_id="trace_public", outcome="approved")

    assert route.called
    req = route.calls[0].request
    assert req.headers["authorization"] == "Bearer jj_from_config"
    body = json.loads(req.content)
    assert body["trace_id"] == "trace_public"
    assert body["outcome"] == "approved"


def test_public_wrapper_raises_without_api_key() -> None:
    from jamjet.cloud import record_outcome as pub_record_outcome
    from jamjet.cloud.config import set_config

    set_config(api_key=None)

    with pytest.raises(RuntimeError, match="not configured"):
        pub_record_outcome(trace_id="t1", outcome="success")
