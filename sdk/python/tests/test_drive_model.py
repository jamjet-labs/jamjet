"""Tests for worker model steps run through the caller's own model.

A model step claimed from the ``model_worker`` queue runs through
``run_claimed_item`` against a real ``JamjetClient`` whose transport plays the
engine, so what is asserted is what went over the wire: the answer on
``/complete`` or the reason on ``/fail``, with the lease fence.
"""

from __future__ import annotations

import asyncio
import json
import math
import sys
import time
import types
from pathlib import Path
from typing import Any

import httpx
import pytest

from jamjet.client import JamjetClient
from jamjet.drive_model import StepContext, run_model_step
from jamjet.model import Model
from jamjet.model.litellm_backend import LiteLLMBackend
from jamjet.model.scripted import ScriptedBackend
from jamjet.model.types import ModelRequest, ModelResponse, parse_model_ref
from jamjet.worker_step import StepResult, run_claimed_item

BASE = "http://runtime"

SEARCH_TOOL = {"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}}


class _Engine:
    """Plays the engine's work-item routes and records every request in order."""

    def __init__(self, *, complete_status: int = 200) -> None:
        self.complete_status = complete_status
        self.requests: list[tuple[str, dict[str, Any], float]] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content) if request.content else {}
        path = request.url.path
        self.requests.append((path, body, time.monotonic()))
        if path.endswith("/heartbeat"):
            return httpx.Response(200, json={"renewed": True, "cancel_requested": False})
        if path.endswith("/complete"):
            if self.complete_status == 409:
                return httpx.Response(409, json={"completed": False, "reason": "stale or invalid lease fence"})
            return httpx.Response(self.complete_status, json={"completed": True})
        if path.endswith("/fail"):
            return httpx.Response(200, json={"failed": True})
        return httpx.Response(404, json={"error": f"unexpected {path}"})

    def bodies(self, suffix: str) -> list[dict[str, Any]]:
        return [b for p, b, _ in self.requests if p.endswith(suffix)]


async def _client(engine: _Engine) -> JamjetClient:
    client = JamjetClient(base_url=BASE)
    await client._client.aclose()
    client._client = httpx.AsyncClient(base_url=BASE, transport=httpx.MockTransport(engine))
    return client


def _item(**payload: Any) -> dict[str, Any]:
    """A claim answer's work item for a worker model step, as the engine hands it out."""
    return {
        "id": "wi-model",
        "execution_id": "exec_model",
        "node_id": "think",
        "queue_type": "model_worker",
        "attempt": 2,
        "lease_fence": 7,
        "lease_secs": 60,
        "idempotency_key": "idem-think-1",
        "payload": {
            "workflow_id": "w",
            "workflow_version": "0.1.0",
            "node_id": "think",
            "model": "openai/gpt-4o",
            "prompt": "Find a venue in Lisbon",
            "claim": "by_execution",
            **payload,
        },
        "budget_left": {"total_tokens": 1200, "input_tokens": None, "output_tokens": None, "cost_usd": 0.42},
    }


class _Message:
    def __init__(self, content: Any = "ok", tool_calls: Any = None) -> None:
        self.content = content
        self.tool_calls = tool_calls


class _Backend:
    """A seam backend that answers with ``answer`` (or calls it) and keeps each request."""

    def __init__(self, answer: Any) -> None:
        self.answer = answer
        self.requests: list[ModelRequest] = []

    async def complete(self, request: ModelRequest) -> Any:
        self.requests.append(request)
        if callable(self.answer):
            return await self.answer(request)
        return self.answer


def _reported(**fields: Any) -> ModelResponse:
    defaults: dict[str, Any] = {
        "message": _Message("Casa do Rio is free."),
        "input_tokens": 812,
        "output_tokens": 95,
        "cost_usd": 0.0031,
        "usage_reported": True,
        "model": "openai/gpt-4o",
    }
    defaults.update(fields)
    return ModelResponse(**defaults)


async def _run(
    answer: Any, *, item: dict[str, Any] | None = None, complete_status: int = 200, **kwargs: Any
) -> tuple[StepResult, _Engine, _Backend]:
    engine = _Engine(complete_status=complete_status)
    backend = _Backend(answer)
    client = await _client(engine)
    try:
        result = await run_claimed_item(
            client,
            "drive-test",
            item or _item(),
            model=Model(backend=backend, ungoverned=True),
            run_key="job:42",
            **kwargs,
        )
    finally:
        await client._client.aclose()
    return result, engine, backend


def _only_failure(engine: _Engine) -> dict[str, Any]:
    assert engine.bodies("/complete") == []
    (failure,) = engine.bodies("/fail")
    assert failure["lease_fence"] == 7
    return failure


# ── the answer ───────────────────────────────────────────────────────────────


async def test_an_answer_is_sent_as_model_result_with_the_fence_and_key() -> None:
    calls = [
        types.SimpleNamespace(
            id="c1", type="function", function=types.SimpleNamespace(name="search", arguments='{"q": "venues"}')
        )
    ]
    result, engine, _ = await _run(
        _reported(message=_Message(None, calls)), item=_item(tools=[SEARCH_TOOL]), heartbeat_interval_s=60
    )
    assert result is StepResult.COMPLETED
    assert engine.bodies("/fail") == []
    (body,) = engine.bodies("/complete")
    assert set(body) == {"lease_fence", "idempotency_key", "duration_ms", "model_result"}
    assert body["lease_fence"] == 7
    assert body["idempotency_key"] == "idem-think-1"
    assert body["model_result"] == {
        "content": "",
        "finish_reason": "tool_calls",
        "model": "openai/gpt-4o",
        "tool_calls": [{"id": "c1", "name": "search", "arguments": {"q": "venues"}}],
        "input_tokens": 812,
        "output_tokens": 95,
        "cost_usd": 0.0031,
    }


async def test_a_409_on_a_model_answer_is_lease_lost_not_a_failure() -> None:
    result, engine, _ = await _run(_reported(), complete_status=409, heartbeat_interval_s=60)
    assert result is StepResult.LEASE_LOST
    assert len(engine.bodies("/complete")) == 1
    assert engine.bodies("/fail") == []


async def test_the_request_is_the_one_the_engine_built() -> None:
    item = _item(
        prompt=None,
        system_prompt="You plan events.",
        messages=[
            {"role": "system", "content": "You plan events."},
            {"role": "user", "content": "Lisbon, 40 guests"},
        ],
        tools=[SEARCH_TOOL],
        temperature=0.5,
        max_tokens=512,
        output_schema="answer",
    )
    del item["payload"]["prompt"]
    _, _, backend = await _run(_reported(), item=item, heartbeat_interval_s=60)
    (request,) = backend.requests
    assert request.ref.litellm_model == "openai/gpt-4o"
    assert request.messages == [
        {"role": "system", "content": "You plan events."},
        {"role": "user", "content": "Lisbon, 40 guests"},
    ]
    assert request.tools == [SEARCH_TOOL]
    assert request.temperature == 0.5
    assert request.max_tokens == 512
    assert request.metadata["output_schema"] == "answer"


async def test_a_prompt_is_sent_alone_after_the_system_prompt() -> None:
    item = _item(system_prompt="You plan events.", messages=[{"role": "user", "content": "not sent"}])
    _, _, backend = await _run(_reported(), item=item, heartbeat_interval_s=60)
    (request,) = backend.requests
    assert request.messages == [
        {"role": "system", "content": "You plan events."},
        {"role": "user", "content": "Find a venue in Lisbon"},
    ]


async def test_a_payload_with_no_prompt_fails_without_calling_the_model() -> None:
    item = _item(prompt_error="prompt template: the run's state has no `city`")
    result, engine, backend = await _run(_reported(), item=item, heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert backend.requests == []
    assert "the run's state has no `city`" in _only_failure(engine)["error"]

    item = _item(system_prompt="You plan events.", messages=[{"role": "system", "content": "You plan events."}])
    del item["payload"]["prompt"]
    result, engine, backend = await _run(_reported(), item=item, heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert backend.requests == []
    assert "no prompt or messages" in _only_failure(engine)["error"]


# ── the brief's table ────────────────────────────────────────────────────────


async def test_a_backend_that_raises_fails_the_item_with_its_reason_and_fence() -> None:
    async def raises(request: ModelRequest) -> Any:
        raise RuntimeError("router refused: quota spent")

    result, engine, _ = await _run(raises, heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    failure = _only_failure(engine)
    assert failure["error"] == "router refused: quota spent"


async def test_a_backend_that_hangs_is_cut_at_the_timeout_and_heartbeats_continue_until_then() -> None:
    cancelled = asyncio.Event()

    async def hangs(request: ModelRequest) -> Any:
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            cancelled.set()
            raise

    started = time.monotonic()
    result, engine, _ = await _run(hangs, heartbeat_interval_s=0.05, model_timeout_s=0.5)
    elapsed = time.monotonic() - started
    assert result is StepResult.FAILED_REPORTED
    assert cancelled.is_set()
    assert 0.5 <= elapsed < 3.0
    failure = _only_failure(engine)
    assert failure["error"] == "the model gave no answer within 0.5 s"
    beats = [t for p, _, t in engine.requests if p.endswith("/heartbeat")]
    (failed_at,) = [t for p, _, t in engine.requests if p.endswith("/fail")]
    assert len(beats) >= 4
    assert all(t <= failed_at for t in beats)
    # Heartbeats ran up to the cut, not only at the start.
    assert beats[-1] - started >= 0.35


async def test_the_model_timeout_is_the_smaller_of_the_payload_and_the_caller() -> None:
    async def hangs(request: ModelRequest) -> Any:
        await asyncio.Event().wait()

    # The payload's `timeout_secs` (the IR's model timeout) is 1 s; the caller allows 30 s.
    result, engine, _ = await _run(hangs, item=_item(timeout_secs=1), heartbeat_interval_s=60, model_timeout_s=30)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"] == "the model gave no answer within 1 s"


async def test_a_backend_with_no_usage_sends_no_cost_and_no_tokens() -> None:
    # Tokens and cost at their defaults (0 and 0.0) but not reported: unknown, never free.
    answer = ModelResponse(message=_Message("Casa do Rio is free."), cost_usd=0.0, model="openai/gpt-4o")
    result, engine, _ = await _run(answer, heartbeat_interval_s=60)
    assert result is StepResult.COMPLETED
    (body,) = engine.bodies("/complete")
    assert body["model_result"] == {
        "content": "Casa do Rio is free.",
        "finish_reason": "stop",
        "model": "openai/gpt-4o",
    }


@pytest.mark.parametrize(("answer", "type_name"), [({"content": "hi"}, "dict"), (None, "NoneType")])
async def test_a_backend_returning_a_dict_or_none_fails_with_its_type(answer: Any, type_name: str) -> None:
    result, engine, _ = await _run(answer, heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"] == f"the model returned {type_name}, not a ModelResponse"


@pytest.mark.parametrize(
    "message",
    [
        None,
        _Message(content=42),
        _Message(tool_calls="search"),
        _Message(tool_calls=[{"id": "c1", "name": "search"}]),
        _Message(tool_calls=[{"name": "search", "arguments": "{}"}]),
        _Message(tool_calls=[{"id": "c1", "arguments": "{}"}]),
    ],
    ids=["no-message", "content-not-text", "calls-not-a-list", "no-arguments", "no-id", "no-name"],
)
async def test_a_message_of_another_shape_fails_the_step(message: Any) -> None:
    result, engine, _ = await _run(_reported(message=message), heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"].startswith("the model's message ")


@pytest.mark.parametrize("cost", [math.nan, -0.01, math.inf, -math.inf, "0.01", True], ids=repr)
async def test_nan_negative_or_infinite_cost_fails_the_step(cost: Any) -> None:
    result, engine, _ = await _run(_reported(cost_usd=cost), heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"].startswith("the model reported a cost_usd that is not ")


@pytest.mark.parametrize(("cost", "sent"), [(0, 0.0), (0.0, 0.0), (0.0031, 0.0031), (None, None)])
async def test_a_cost_of_zero_or_more_is_sent_and_none_is_left_out(cost: Any, sent: Any) -> None:
    result, engine, _ = await _run(_reported(cost_usd=cost), heartbeat_interval_s=60)
    assert result is StepResult.COMPLETED
    (body,) = engine.bodies("/complete")
    assert body["model_result"]["input_tokens"] == 812
    assert body["model_result"]["output_tokens"] == 95
    if sent is None:
        assert "cost_usd" not in body["model_result"]
    else:
        assert body["model_result"]["cost_usd"] == sent


@pytest.mark.parametrize(
    "tokens",
    [{"input_tokens": True}, {"output_tokens": False}, {"input_tokens": -1}, {"output_tokens": 9.5}],
    ids=["input-true", "output-false", "negative", "fractional"],
)
async def test_bool_tokens_are_refused(tokens: dict[str, Any]) -> None:
    result, engine, _ = await _run(_reported(**tokens), heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"].startswith("the model reported ")


async def test_the_backend_sees_run_ids_and_the_idempotency_key() -> None:
    _, engine, backend = await _run(_reported(), heartbeat_interval_s=60)
    (request,) = backend.requests
    assert request.metadata["jamjet"] == {
        "execution_id": "exec_model",
        "node_id": "think",
        "item_id": "wi-model",
        "attempt": 2,
        "idempotency_key": "idem-think-1",
        "run_key": "job:42",
        "budget": {"total_tokens": 1200, "cost_usd": 0.42},
    }
    (body,) = engine.bodies("/complete")
    assert body["idempotency_key"] == "idem-think-1"


async def test_the_model_used_is_what_the_backend_reported() -> None:
    result, engine, _ = await _run(_reported(model="openai/gpt-4o-mini"), heartbeat_interval_s=60)
    assert result is StepResult.COMPLETED
    (body,) = engine.bodies("/complete")
    assert body["model_result"]["model"] == "openai/gpt-4o-mini"


@pytest.mark.parametrize("model", [None, "", 7])
async def test_a_backend_that_names_no_model_fails_the_step(model: Any) -> None:
    result, engine, _ = await _run(_reported(model=model), heartbeat_interval_s=60)
    assert result is StepResult.FAILED_REPORTED
    assert _only_failure(engine)["error"] == "the model did not report which model answered"


async def test_a_model_that_is_not_the_seam_is_a_type_error() -> None:
    ctx = StepContext(
        execution_id="exec_model",
        node_id="think",
        item_id="wi-model",
        attempt=0,
        idempotency_key=None,
        run_key=None,
        budget=None,
    )
    with pytest.raises(TypeError, match="jamjet.model.Model"):
        await run_model_step(_Backend(_reported()), _item()["payload"], ctx, timeout_s=5)  # type: ignore[arg-type]

    # Through the executor, with no model at all, the item fails with that reason.
    engine = _Engine()
    client = await _client(engine)
    try:
        result = await run_claimed_item(client, "drive-test", _item(), heartbeat_interval_s=60)
    finally:
        await client._client.aclose()
    assert result is StepResult.FAILED_REPORTED
    assert "jamjet.model.Model" in _only_failure(engine)["error"]


# ── the shipped backends ─────────────────────────────────────────────────────


class _Usage:
    prompt_tokens = 12
    completion_tokens = 5


def _install_fake_litellm(monkeypatch: pytest.MonkeyPatch, *, usage: Any) -> None:
    message = types.SimpleNamespace(content="hello", tool_calls=None)
    response = types.SimpleNamespace(
        choices=[types.SimpleNamespace(message=message)],
        usage=usage,
        # What a provider calls its model is not what was asked for, and is not reported.
        model="gpt-4o-2024-08-06",
    )

    async def acompletion(**kwargs: Any) -> Any:
        return response

    def completion_cost(*, completion_response: Any) -> float:
        return 0.0009

    fake = types.ModuleType("litellm")
    fake.acompletion = acompletion  # type: ignore[attr-defined]
    fake.completion_cost = completion_cost  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "litellm", fake)


async def test_the_shipped_backends_report_usage(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    _install_fake_litellm(monkeypatch, usage=_Usage())
    request = ModelRequest(ref=parse_model_ref("openai/gpt-4o"), messages=[{"role": "user", "content": "hi"}])
    answer = await LiteLLMBackend().complete(request)
    assert answer.usage_reported is True
    assert answer.model == "openai/gpt-4o"
    assert (answer.input_tokens, answer.output_tokens, answer.cost_usd) == (12, 5, 0.0009)

    # No usage on the answer: nothing reported, so the engine cannot read it as free.
    _install_fake_litellm(monkeypatch, usage=None)
    assert (await LiteLLMBackend().complete(request)).usage_reported is False

    script = tmp_path / "turns.yaml"
    script.write_text("turns:\n  - content: done\n    usage: {input_tokens: 9, output_tokens: 3}\n")
    scripted = ModelRequest(ref=parse_model_ref(f"scripted/{script}"), messages=[{"role": "user", "content": "hi"}])
    answer = await ScriptedBackend().complete(scripted)
    assert answer.usage_reported is True
    assert answer.model == f"scripted/{script}"
    assert (answer.input_tokens, answer.output_tokens, answer.cost_usd) == (9, 3, 0.0)
