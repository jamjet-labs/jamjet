"""Tests for drive mode: the caller's process works its own run's steps.

``drive_run`` claims the run's items by execution, runs each one with the
step executor ``jamjet worker`` uses, and leaves by the exit rules. The engine
here is a script played over respx: each claim gets the next answer in the
script (the last one repeats), so what is asserted is what went over the wire.
"""

from __future__ import annotations

import asyncio
import json
import multiprocessing
import re
import subprocess
import sys
import types
from typing import Any

import httpx
import pytest
import respx

from jamjet import drive, transport
from jamjet.agents.agent import Agent
from jamjet.client import JamjetAPIError, JamjetClient
from jamjet.drive import (
    RunBlocked,
    RunCancelled,
    RunInterrupted,
    RunStalled,
    RunWaiting,
    drive_run,
    driver_worker_id,
)
from jamjet.model import Model
from jamjet.model.types import ModelRequest, ModelResponse
from jamjet.transport import EngineFeatureMissing
from jamjet.worker_context import current_work

BASE = "http://engine.test:7700"
QUEUES = ["python_tool", "model_worker"]
ALL_FEATURES = ["run_key", "python_fn_by_execution", "worker_model_steps"]

RUNNING = {"execution_id": "exec_1", "status": "running", "updated_at": "t1", "current_state": {}}
COMPLETED = {
    "execution_id": "exec_1",
    "status": "completed",
    "current_state": {
        "messages": [
            {"role": "system", "content": "Answer briefly."},
            {"role": "user", "content": "Find a venue"},
        ],
        "last_model_output": "Casa do Rio is free.",
    },
}
ENDED_COMPLETED = {"claimed": False, "reason": "not_found", "status": "completed"}


@pytest.fixture(autouse=True)
def _env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)
    # run_durable signs its audit chain; a key keeps the unsigned warning out.
    monkeypatch.setenv("JAMJET_AUDIT_SIGNING_KEY", "test-only-signing-key")


@pytest.fixture(autouse=True)
def _no_transport_sleep(monkeypatch: pytest.MonkeyPatch) -> None:
    async def fake_sleep(seconds: float) -> None:
        return None

    monkeypatch.setattr(transport, "_sleep", fake_sleep)


class Clock:
    """The drive loop's clock and pause: a pause moves the clock, nothing sleeps."""

    def __init__(self) -> None:
        self.now = 1000.0
        self.slept: list[float] = []

    def monotonic(self) -> float:
        return self.now

    async def sleep(self, seconds: float) -> None:
        self.slept.append(seconds)
        self.now += seconds
        await asyncio.sleep(0)


@pytest.fixture
def clock(monkeypatch: pytest.MonkeyPatch) -> Clock:
    c = Clock()
    monkeypatch.setattr(drive, "_monotonic", c.monotonic)
    monkeypatch.setattr(drive, "_sleep", c.sleep)
    return c


@pytest.fixture
def tools(monkeypatch: pytest.MonkeyPatch) -> types.SimpleNamespace:
    """A module the tool items name, with handlers the tests control."""
    module = types.ModuleType("drive_test_tools")
    state = types.SimpleNamespace(
        module=module, calls=[], keys=[], release=asyncio.Event(), cancelled=False, on_start=None
    )

    async def dispatch(payload: dict[str, Any]) -> dict[str, Any]:
        state.calls.append(payload)
        work = current_work()
        state.keys.append((work.run_key, work.idempotency_key))
        return {"messages": [{"role": "tool", "content": "Casa do Rio: free on Friday"}]}

    async def slow(payload: dict[str, Any]) -> dict[str, Any]:
        state.calls.append(payload)
        if state.on_start is not None:
            state.on_start()
        try:
            await state.release.wait()
        except asyncio.CancelledError:
            state.cancelled = True
            raise
        return {"done": True}

    module.dispatch = dispatch  # type: ignore[attr-defined]
    module.slow = slow  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "drive_test_tools", module)
    return state


def tool_item(item_id: str = "wi-tools", *, function: str = "dispatch", lease_secs: int = 60, **fields: Any) -> dict:
    item = {
        "id": item_id,
        "execution_id": "exec_1",
        "node_id": "__tools_0__",
        "queue_type": "python_tool",
        "attempt": 0,
        "lease_fence": 3,
        "lease_secs": lease_secs,
        "idempotency_key": f"idem-{item_id}",
        "payload": {"module": "drive_test_tools", "function": function, "input": {"q": "venues"}},
    }
    item.update(fields)
    return {"claimed": True, "work_item": item}


def model_item(item_id: str = "wi-model") -> dict:
    return {
        "claimed": True,
        "work_item": {
            "id": item_id,
            "execution_id": "exec_1",
            "node_id": "__model_0__",
            "queue_type": "model_worker",
            "attempt": 0,
            "lease_fence": 5,
            "lease_secs": 60,
            "idempotency_key": f"idem-{item_id}",
            "payload": {
                "workflow_id": "helper",
                "node_id": "__model_0__",
                "model": "openai/gpt-4o",
                "prompt": "Find a venue",
                "claim": "by_execution",
            },
            "budget_left": {"total_tokens": None, "input_tokens": None, "output_tokens": None, "cost_usd": None},
        },
    }


class ScriptedEngine:
    """Plays the engine's routes. ``claims`` and ``executions`` are answered in
    order, the last one again and again; an entry is a JSON body or a
    ``httpx.Response``."""

    def __init__(
        self,
        *,
        claims: list[Any],
        executions: list[Any] | None = None,
        health: list[Any] | None = None,
        complete_status: dict[str, int] | None = None,
        heartbeat: dict[str, Any] | None = None,
    ) -> None:
        self.claims = list(claims)
        self.executions = list(executions or [COMPLETED])
        self.health = list(health or [{"status": "ok", "features": ALL_FEATURES}])
        self.complete_status = complete_status or {}
        self.heartbeat = heartbeat or {"renewed": True, "cancel_requested": False}
        self.requests: list[tuple[str, str, Any]] = []

    @staticmethod
    def _next(script: list[Any]) -> httpx.Response:
        answer = script.pop(0) if len(script) > 1 else script[0]
        return answer if isinstance(answer, httpx.Response) else httpx.Response(200, json=answer)

    def __call__(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content) if request.content else None
        path = request.url.path
        self.requests.append((request.method, path, body))
        if path == "/health":
            return self._next(self.health)
        if path == "/workflows":
            return httpx.Response(201, json={"workflow_id": "helper"})
        if path == "/executions" and request.method == "POST":
            started: dict[str, Any] = {"execution_id": "exec_1"}
            if body.get("run_key"):
                started["run_key"] = body["run_key"]
            return httpx.Response(201, json=started)
        if path == "/work-items/claim":
            return self._next(self.claims)
        if path.endswith("/heartbeat"):
            return httpx.Response(200, json=self.heartbeat)
        if path.endswith("/complete"):
            item_id = path.split("/")[2]
            status = self.complete_status.get(item_id, 200)
            if status == 409:
                return httpx.Response(409, json={"completed": False, "reason": "stale or invalid lease fence"})
            return httpx.Response(status, json={"completed": True})
        if path.endswith("/fail"):
            return httpx.Response(200, json={"failed": True})
        if path == "/executions/exec_1/events":
            return httpx.Response(200, json={"events": []})
        if path == "/executions/exec_1":
            return self._next(self.executions)
        return httpx.Response(404, json={"error": f"unexpected {request.method} {path}"})

    def sent(self, suffix: str) -> list[Any]:
        return [b for _, p, b in self.requests if p.endswith(suffix)]

    def paths(self) -> list[str]:
        return [f"{m} {p}" for m, p, _ in self.requests]


def claim_body(lease_secs: int | None = None) -> dict[str, Any]:
    body: dict[str, Any] = {
        "worker_id": driver_worker_id(),
        "queue_types": QUEUES,
        "execution_id": "exec_1",
        "wait_ms": 10000,
    }
    if lease_secs is not None:
        body["lease_secs"] = lease_secs
    return body


class Backend:
    """A seam backend that answers every request the same way."""

    def __init__(self) -> None:
        self.requests: list[ModelRequest] = []

    async def complete(self, request: ModelRequest) -> ModelResponse:
        self.requests.append(request)
        message = types.SimpleNamespace(content="Casa do Rio is free.", tool_calls=None)
        return ModelResponse(
            message=message,
            input_tokens=40,
            output_tokens=8,
            cost_usd=0.0004,
            usage_reported=True,
            model="openai/gpt-4o",
        )


def caller_model() -> Model:
    return Model(backend=Backend(), ungoverned=True)


def agent() -> Agent:
    return Agent("helper", model="openai/gpt-4o", tools=[], instructions="Answer briefly.")


async def drive_with(engine: ScriptedEngine, **kwargs: Any) -> dict[str, Any]:
    with respx.mock(assert_all_called=False) as mock:
        mock.route(host="engine.test").mock(side_effect=engine)
        async with JamjetClient(BASE) as client:
            kwargs.setdefault("queues", QUEUES)
            kwargs.setdefault("model", caller_model())
            kwargs.setdefault("run_key", None)
            return await drive_run(client, "exec_1", **kwargs)


# ── the loop ─────────────────────────────────────────────────────────────────


async def test_drive_runs_tool_and_model_steps_until_completed(tools: types.SimpleNamespace, clock: Clock) -> None:
    engine = ScriptedEngine(claims=[model_item(), tool_item(), ENDED_COMPLETED])
    backend = Backend()
    with respx.mock(assert_all_called=False) as mock:
        mock.route(host="engine.test").mock(side_effect=engine)
        result = await agent().run_durable(
            "Find a venue",
            runtime_url=BASE,
            run_key="job:7",
            drive=True,
            model=Model(backend=backend, ungoverned=True),
            lease_secs=90,
        )

    assert result.output == "Casa do Rio is free."
    assert result.execution_id == "exec_1"
    # The features, then the register and the start, then claims for this run only.
    assert engine.paths()[:3] == ["GET /health", "POST /workflows", "POST /executions"]
    # The registered graph hands every model step and every tool step to this run's worker.
    (registered,) = engine.sent("/workflows")
    kinds = {node_id: node["kind"] for node_id, node in registered["ir"]["nodes"].items()}
    assert {i: k["executor"] for i, k in kinds.items() if k["type"] == "model"} == {
        f"__model_{t}__": "worker" for t in range(9)
    }
    assert {i: k["claim"] for i, k in kinds.items() if k["type"] == "python_fn"} == {
        f"__tools_{t}__": "by_execution" for t in range(8)
    }
    assert engine.sent("/work-items/claim") == [claim_body(90)] * 3
    # The model step went to the caller's model and its answer to the engine.
    assert [r.messages for r in backend.requests] == [[{"role": "user", "content": "Find a venue"}]]
    (model_done,) = engine.sent("/work-items/wi-model/complete")
    assert isinstance(model_done.pop("duration_ms"), int)
    assert model_done == {
        "lease_fence": 5,
        "idempotency_key": "idem-wi-model",
        "model_result": {
            "content": "Casa do Rio is free.",
            "finish_reason": "stop",
            "model": "openai/gpt-4o",
            "input_tokens": 40,
            "output_tokens": 8,
            "cost_usd": 0.0004,
        },
    }
    # The tool step ran here and its result went back under its fence and key.
    assert tools.calls == [{"q": "venues"}]
    assert tools.keys == [("job:7", "idem-wi-tools")]
    (tool_done,) = engine.sent("/work-items/wi-tools/complete")
    assert tool_done["lease_fence"] == 3
    assert tool_done["idempotency_key"] == "idem-wi-tools"
    assert tool_done["state_patch"] == {"messages": [{"role": "tool", "content": "Casa do Rio: free on Friday"}]}
    assert engine.sent("/fail") == []
    # No pause while there was work.
    assert clock.slept == []


@pytest.mark.parametrize(
    "row",
    [
        "item_then_lease_lost",
        "approval_required",
        "blocked",
        "refused",
        "claimed_by_other",
        "not_ready_then_completed",
        "not_ready_then_failed",
        "not_ready_then_cancelled",
        "not_ready_stalls",
        "no_reason",
        "engine_403",
    ],
)
async def test_each_claim_answer_takes_its_row_in_the_table(
    row: str, tools: types.SimpleNamespace, clock: Clock
) -> None:
    if row == "item_then_lease_lost":
        # An item is run; a 409 on its completion is a lost lease, and the loop goes on.
        engine = ScriptedEngine(claims=[tool_item(), ENDED_COMPLETED], complete_status={"wi-tools": 409})
        execution = await drive_with(engine)
        assert execution == COMPLETED
        assert len(engine.sent("/work-items/wi-tools/complete")) == 1
        assert engine.sent("/fail") == []
        assert len(engine.sent("/work-items/claim")) == 2

    elif row == "approval_required":
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": "approval_required"}])
        with pytest.raises(RunWaiting) as waiting:
            await drive_with(engine)
        assert waiting.value.execution_id == "exec_1"
        assert waiting.value.waiting_for == "approval"
        assert waiting.value.node_id is None
        assert len(engine.sent("/work-items/claim")) == 1

    elif row in ("blocked", "refused"):
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": row}])
        with pytest.raises(RunBlocked) as blocked:
            await drive_with(engine)
        assert blocked.value.reason == row
        assert blocked.value.execution_id == "exec_1"
        assert f"exec_1 ({row})" in str(blocked.value)
        assert len(engine.sent("/work-items/claim")) == 1

    elif row == "claimed_by_other":
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": "claimed_by_other"}])
        with pytest.raises(RunWaiting) as waiting:
            await drive_with(engine, stall_after_s=3.0)
        assert waiting.value.waiting_for == "worker"
        assert waiting.value.execution_id == "exec_1"
        # Claimed again every second until the bound.
        assert len(engine.sent("/work-items/claim")) == 4
        assert clock.slept == [1.0, 1.0, 1.0]

    elif row == "not_ready_then_completed":
        engine = ScriptedEngine(
            claims=[{"claimed": False, "reason": "not_ready"}],
            executions=[RUNNING, COMPLETED],
        )
        execution = await drive_with(engine)
        assert execution == COMPLETED
        assert len(engine.sent("/work-items/claim")) == 2

    elif row == "not_ready_then_failed":
        failed = {"execution_id": "exec_1", "status": "failed", "current_state": {}}
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_ready"}], executions=[failed])
        execution = await drive_with(engine)
        assert execution == failed

    elif row == "not_ready_then_cancelled":
        cancelled = {"execution_id": "exec_1", "status": "cancelled", "current_state": {}}
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_ready"}], executions=[cancelled])
        with pytest.raises(RunCancelled) as stopped:
            await drive_with(engine)
        assert stopped.value.execution_id == "exec_1"

    elif row == "not_ready_stalls":
        engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_ready"}], executions=[RUNNING])
        with pytest.raises(RunStalled) as stalled:
            await drive_with(engine, stall_after_s=3.0)
        assert stalled.value.execution_id == "exec_1"
        assert "no progress in 3 s" in str(stalled.value)
        assert len(engine.sent("/work-items/claim")) == 4
        assert clock.slept == [1.0, 1.0, 1.0]

    elif row == "no_reason":
        # Draining, or an engine that does not say: pause a second and claim again.
        engine = ScriptedEngine(claims=[{"claimed": False}, {"claimed": False}, ENDED_COMPLETED])
        execution = await drive_with(engine)
        assert execution == COMPLETED
        assert len(engine.sent("/work-items/claim")) == 3
        assert clock.slept == [1.0, 1.0]

    elif row == "engine_403":
        engine = ScriptedEngine(claims=[httpx.Response(403, json={"error": "role reviewer may not claim"})])
        with pytest.raises(JamjetAPIError) as refused:
            await drive_with(engine)
        assert refused.value.response.status_code == 403
        assert len(engine.sent("/work-items/claim")) == 1
        assert clock.slept == []


async def test_progress_in_the_run_resets_the_stall_bound(clock: Clock) -> None:
    # The engine is working: every read shows a newer run, so no stall.
    advancing = [{**RUNNING, "updated_at": f"t{n}"} for n in range(6)]
    engine = ScriptedEngine(
        claims=[{"claimed": False, "reason": "not_ready"}],
        executions=[*advancing, COMPLETED],
    )
    execution = await drive_with(engine, stall_after_s=2.0)
    assert execution == COMPLETED
    assert len(engine.sent("/work-items/claim")) == 7


async def test_an_ended_run_or_a_run_not_ours_ends_the_drive(clock: Clock) -> None:
    # A run that ended answers not_found with its status.
    engine = ScriptedEngine(
        claims=[{"claimed": False, "reason": "not_found", "status": "failed"}],
        executions=[{"execution_id": "exec_1", "status": "failed", "current_state": {}}],
    )
    execution = await drive_with(engine)
    assert execution["status"] == "failed"

    # A live run this caller may not claim for.
    engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_found"}], executions=[RUNNING])
    with pytest.raises(RunBlocked) as blocked:
        await drive_with(engine)
    assert blocked.value.reason == "not_found"
    assert "only the token that started the run" in str(blocked.value)


async def test_an_item_of_another_run_is_not_run(tools: types.SimpleNamespace, clock: Clock) -> None:
    other = tool_item()
    other["work_item"]["execution_id"] = "exec_other"
    engine = ScriptedEngine(claims=[other])
    with pytest.raises(RunBlocked) as blocked:
        await drive_with(engine)
    assert "exec_other" in str(blocked.value)
    assert tools.calls == []
    assert engine.sent("/complete") == []
    assert engine.sent("/fail") == []


async def test_claimed_by_other_takes_the_item_after_the_lease_lapses(
    tools: types.SimpleNamespace, clock: Clock
) -> None:
    # A driver that died holds the item until its lease lapses; then it is ours.
    other = {"claimed": False, "reason": "claimed_by_other"}
    engine = ScriptedEngine(claims=[other, other, other, tool_item(), ENDED_COMPLETED])
    execution = await drive_with(engine, stall_after_s=30.0)
    assert execution == COMPLETED
    assert len(engine.sent("/work-items/claim")) == 5
    assert clock.slept == [1.0, 1.0, 1.0]
    assert tools.calls == [{"q": "venues"}]
    assert len(engine.sent("/work-items/wi-tools/complete")) == 1


async def test_a_stall_names_an_overdue_tick(clock: Clock) -> None:
    overdue = {"status": "ok", "background": "off", "tick_overdue": True}
    engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_ready"}], executions=[RUNNING], health=[overdue])
    with pytest.raises(RunStalled) as stalled:
        await drive_with(engine, stall_after_s=2.0)
    message = str(stalled.value)
    assert "tick overdue" in message
    assert "POST /ops/tick" in message

    # An engine whose ticks are on time: the stall does not blame the tick.
    on_time = {"status": "ok", "background": "off", "tick_overdue": False}
    engine = ScriptedEngine(claims=[{"claimed": False, "reason": "not_ready"}], executions=[RUNNING], health=[on_time])
    with pytest.raises(RunStalled) as stalled:
        await drive_with(engine, stall_after_s=2.0)
    assert "tick" not in str(stalled.value)


# ── cancel and stop ──────────────────────────────────────────────────────────


async def test_cancel_mid_step_raises_and_reports_nothing(tools: types.SimpleNamespace) -> None:
    # A lease of 3 s: the first heartbeat goes after 1 s and says the run was cancelled.
    engine = ScriptedEngine(
        claims=[tool_item(function="slow", lease_secs=3), ENDED_COMPLETED],
        heartbeat={"renewed": True, "cancel_requested": True},
    )
    with pytest.raises(RunCancelled) as stopped:
        await drive_with(engine)
    assert stopped.value.execution_id == "exec_1"
    assert tools.cancelled is True
    assert len(engine.sent("/heartbeat")) == 1
    assert engine.sent("/complete") == []
    assert engine.sent("/fail") == []
    assert len(engine.sent("/work-items/claim")) == 1


@pytest.mark.parametrize("finishes", ["in_grace", "past_grace"])
async def test_a_stop_signal_finishes_the_step_in_grace_or_leaves_the_lease(
    finishes: str, tools: types.SimpleNamespace, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(drive, "STOP_GRACE_S", 0.5)
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()

    def stop_now() -> None:
        stop.set()
        if finishes == "in_grace":
            loop.call_later(0.1, tools.release.set)

    tools.on_start = stop_now
    engine = ScriptedEngine(claims=[tool_item(function="slow"), ENDED_COMPLETED])
    with pytest.raises(RunInterrupted) as interrupted:
        await drive_with(engine, stop=stop)
    assert interrupted.value.execution_id == "exec_1"
    # Nothing is claimed after the stop.
    assert len(engine.sent("/work-items/claim")) == 1
    assert engine.sent("/fail") == []
    if finishes == "in_grace":
        assert len(engine.sent("/work-items/wi-tools/complete")) == 1
        assert tools.cancelled is False
    else:
        # The step is abandoned: no result, no failure, no more heartbeats.
        assert engine.sent("/complete") == []
        assert tools.cancelled is True
        assert "left to lapse" in str(interrupted.value)


async def test_a_stop_signal_before_a_claim_claims_nothing() -> None:
    stop = asyncio.Event()
    stop.set()
    engine = ScriptedEngine(claims=[ENDED_COMPLETED])
    with pytest.raises(RunInterrupted):
        await drive_with(engine, stop=stop)
    assert engine.requests == []


# ── the worker id ────────────────────────────────────────────────────────────


def _child_worker_id(queue: Any) -> None:
    queue.put(driver_worker_id())


def test_the_worker_id_is_unique_per_process() -> None:
    pattern = re.compile(r"drive-[0-9a-f]{12}")
    mine = driver_worker_id()
    assert pattern.fullmatch(mine)
    # The same for every run this process drives.
    assert driver_worker_id() == mine

    # Another process gets another id.
    code = "from jamjet.drive import driver_worker_id; print(driver_worker_id())"
    others = [
        subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=True).stdout.strip()
        for _ in range(2)
    ]
    for other in others:
        assert pattern.fullmatch(other)
    assert len({mine, *others}) == 3

    # A forked child too: two processes never share a lease.
    if "fork" in multiprocessing.get_all_start_methods():
        ctx = multiprocessing.get_context("fork")
        queue = ctx.Queue()
        child = ctx.Process(target=_child_worker_id, args=(queue,))
        child.start()
        forked = queue.get(timeout=30)
        child.join(timeout=30)
        assert pattern.fullmatch(forked)
        assert forked != mine


# ── auth and features ────────────────────────────────────────────────────────


async def test_an_engine_401_stops_driving_at_once(clock: Clock) -> None:
    engine = ScriptedEngine(claims=[httpx.Response(401, json={"error": "invalid or expired token"})])
    with pytest.raises(JamjetAPIError) as refused:
        await drive_with(engine)
    assert refused.value.response.status_code == 401
    assert len(engine.sent("/work-items/claim")) == 1
    assert clock.slept == []


@pytest.mark.parametrize(
    ("listed", "run_key", "missing"),
    [
        ([], None, "python_fn_by_execution"),
        (["python_fn_by_execution"], None, "worker_model_steps"),
        (["python_fn_by_execution", "worker_model_steps"], "job:7", "run_key"),
    ],
)
async def test_drive_without_the_features_raises_before_registering(
    listed: list[str], run_key: str | None, missing: str
) -> None:
    engine = ScriptedEngine(claims=[ENDED_COMPLETED], health=[{"status": "ok", "features": listed}])
    with respx.mock(assert_all_called=False) as mock:
        mock.route(host="engine.test").mock(side_effect=engine)
        with pytest.raises(EngineFeatureMissing) as exc:
            await agent().run_durable("Find a venue", runtime_url=BASE, run_key=run_key, drive=True)
    assert exc.value.feature == missing
    assert engine.paths() == ["GET /health"]


async def test_model_without_drive_is_a_type_error() -> None:
    engine = ScriptedEngine(claims=[ENDED_COMPLETED])
    with respx.mock(assert_all_called=False) as mock:
        mock.route(host="engine.test").mock(side_effect=engine)
        with pytest.raises(TypeError, match="drive=True"):
            await agent().run_durable("Find a venue", runtime_url=BASE, model=caller_model())
        with pytest.raises(TypeError, match="drive=True"):
            await agent().run_durable("Find a venue", runtime_url=BASE, lease_secs=60)
        with pytest.raises(TypeError, match="jamjet.model.Model"):
            await agent().run_durable("Find a venue", runtime_url=BASE, drive=True, model=Backend())  # type: ignore[arg-type]
    assert engine.requests == []
