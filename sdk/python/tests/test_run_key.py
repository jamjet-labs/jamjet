"""Starting a run by key, and only on an engine that honours the key.

An engine that does not know ``run_key`` drops it and starts a new run on
every retry, so the client checks ``/health`` before the first keyed start and
writes nothing when the engine does not list ``run_key``. The fake engine here
follows the engine's rules: a new key is 201, the same key with the same
workflow, version and input is 200 with the same run, another input is 409,
and ``GET /executions?run_key=`` echoes the key.
"""

from __future__ import annotations

import json
from typing import Any

import httpx
import pytest
import respx

from jamjet import transport
from jamjet.agents.agent import Agent
from jamjet.client import JamjetAPIError, JamjetClient
from jamjet.transport import EngineFeatureMissing, OutcomeUnknown, RunKeyConflict

BASE = "http://engine.test:7700"


@pytest.fixture(autouse=True)
def _env(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("JAMJET_TOKEN", raising=False)
    # run_durable signs its audit chain; a key keeps the unsigned warning out.
    monkeypatch.setenv("JAMJET_AUDIT_SIGNING_KEY", "test-only-signing-key")


@pytest.fixture(autouse=True)
def _no_sleep(monkeypatch: pytest.MonkeyPatch) -> None:
    async def fake_sleep(seconds: float) -> None:
        return None

    monkeypatch.setattr(transport, "_sleep", fake_sleep)


class FakeEngine:
    """Enough of an engine for keyed starts.

    ``honours`` False is an engine that ignores the key: it starts a new run
    for every start and lists every run for a lookup. ``drop_answers`` is how
    many starts take effect and then lose their answer.
    """

    def __init__(
        self,
        mock: respx.MockRouter,
        *,
        features: list[str] | None = None,
        commit: str | None = None,
        honours: bool = True,
        drop_answers: int = 0,
    ) -> None:
        self.honours = honours
        self.drop_answers = drop_answers
        self.started: list[str] = []
        self.keys: dict[str, tuple[str, str | None, Any, str]] = {}
        health: dict[str, Any] = {"status": "ok", "version": "0.14.0"}
        if features is not None:
            health["features"] = features
        if commit is not None:
            health["commit"] = commit
        self.health = mock.get(f"{BASE}/health").respond(200, json=health)
        self.register = mock.post(f"{BASE}/workflows").respond(201, json={"workflow_id": "w"})
        self.start = mock.post(f"{BASE}/executions").mock(side_effect=self._start)
        self.lookup = mock.get(f"{BASE}/executions").mock(side_effect=self._lookup)

    def _new_run(self) -> str:
        execution_id = f"exec_{len(self.started) + 1}"
        self.started.append(execution_id)
        return execution_id

    def _start(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        key = body.get("run_key") if self.honours else None
        if key is None:
            answer = httpx.Response(201, json={"execution_id": self._new_run()})
        elif key in self.keys:
            workflow_id, version, given, execution_id = self.keys[key]
            if (workflow_id, version, given) != (body["workflow_id"], body.get("workflow_version"), body["input"]):
                return httpx.Response(
                    409, json={"error": f"run_key '{key}' names a run of another workflow, version or input"}
                )
            answer = httpx.Response(
                200,
                json={
                    "execution_id": execution_id,
                    "current_execution_id": execution_id,
                    "run_key": key,
                    "created": False,
                    "status": "running",
                },
            )
        else:
            execution_id = self._new_run()
            self.keys[key] = (body["workflow_id"], body.get("workflow_version"), body["input"], execution_id)
            answer = httpx.Response(201, json={"execution_id": execution_id, "run_key": key, "created": True})
        if self.drop_answers:
            self.drop_answers -= 1
            raise httpx.RemoteProtocolError("Server disconnected without sending a response.", request=request)
        return answer

    def _lookup(self, request: httpx.Request) -> httpx.Response:
        key = request.url.params.get("run_key")
        if not self.honours or key is None:
            return httpx.Response(200, json={"executions": [{"execution_id": e} for e in self.started]})
        rows = [{"execution_id": self.keys[key][3], "status": "running"}] if key in self.keys else []
        return httpx.Response(200, json={"run_key": key, "executions": rows})


def _agent() -> Agent:
    return Agent("helper", model="anthropic/claude-sonnet-4-6", tools=[], instructions="Answer briefly.")


def _writes(mock: respx.MockRouter) -> list[str]:
    return [f"{c.request.method} {c.request.url.path}" for c in mock.calls if c.request.method != "GET"]


async def test_a_key_on_an_engine_without_the_feature_writes_nothing() -> None:
    # An engine from before keyed starts: no features in /health at all.
    with respx.mock(assert_all_called=False) as mock:
        FakeEngine(mock, honours=False)
        with pytest.raises(EngineFeatureMissing) as durable:
            await _agent().run_durable("hi", runtime_url=BASE, run_key="job:1")
        assert _writes(mock) == []
    assert durable.value.feature == "run_key"
    assert durable.value.engine_commit is None

    # An engine that lists other features, and reports its commit.
    with respx.mock(assert_all_called=False) as mock:
        FakeEngine(mock, features=["ops_tick"], commit="4f1c2d9", honours=False)
        async with JamjetClient(BASE) as client:
            with pytest.raises(EngineFeatureMissing) as direct:
                await client.start_execution("w", {"q": "hi"}, run_key="job:1")
        assert _writes(mock) == []
    assert direct.value.feature == "run_key"
    assert direct.value.engine_commit == "4f1c2d9"


async def test_same_key_same_input_returns_the_existing_run() -> None:
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        async with JamjetClient(BASE) as client:
            first = await client.start_execution("w", {"q": "hi"}, workflow_version="1.0.0", run_key="job:1")
        # A retried job: a new process, a new client, the same key.
        async with JamjetClient(BASE) as client:
            again = await client.start_execution("w", {"q": "hi"}, workflow_version="1.0.0", run_key="job:1")
    assert first["execution_id"] == "exec_1"
    assert first["created"] is True
    assert engine.start.calls[0].response.status_code == 201
    assert again["execution_id"] == "exec_1"
    assert again["created"] is False
    assert engine.start.calls[1].response.status_code == 200
    assert engine.started == ["exec_1"]


async def test_same_key_other_input_is_a_conflict() -> None:
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        async with JamjetClient(BASE) as client:
            await client.start_execution("w", {"q": "hi"}, run_key="job:1")
            with pytest.raises(RunKeyConflict) as exc:
                await client.start_execution("w", {"q": "bye"}, run_key="job:1")
    error = exc.value
    assert isinstance(error, JamjetAPIError)
    assert error.response.status_code == 409
    assert error.existing_execution_id == "exec_1"
    assert error.detail == "run_key 'job:1' names a run of another workflow, version or input"
    assert engine.start.call_count == 2
    assert engine.started == ["exec_1"]


async def test_a_409_for_another_reason_is_not_a_key_conflict() -> None:
    # A version that cannot be started is 409 too; the key holds no run.
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        engine.start.mock(
            side_effect=None,
            return_value=httpx.Response(409, json={"error": "workflow w v1.0.0 cannot be started: bad entry"}),
        )
        async with JamjetClient(BASE) as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.start_execution("w", {"q": "hi"}, run_key="job:1")
    assert not isinstance(exc.value, RunKeyConflict)
    assert exc.value.detail == "workflow w v1.0.0 cannot be started: bad entry"


async def test_an_answer_without_the_echo_is_refused() -> None:
    # The engine lists run_key but answers as an engine that dropped it.
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"], honours=False)
        async with JamjetClient(BASE) as client:
            with pytest.raises(JamjetAPIError) as exc:
                await client.start_execution("w", {"q": "hi"}, run_key="job:1")
    assert engine.started == ["exec_1"]
    message = str(exc.value)
    assert "did not echo run_key 'job:1'" in message
    assert "exec_1" in message


async def test_a_start_with_a_key_is_retried_after_a_dropped_connection() -> None:
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"], drop_answers=1)
        async with JamjetClient(BASE) as client:
            started = await client.start_execution("w", {"q": "hi"}, run_key="job:1")
    assert engine.start.call_count == 2
    assert engine.started == ["exec_1"]
    assert started["execution_id"] == "exec_1"


async def test_a_failed_only_start_is_not_repeated_after_a_lost_answer() -> None:
    # With failed_only a second send could start a new run if the first one
    # failed in between, so a lost answer is reported, not repeated.
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"], drop_answers=1)
        async with JamjetClient(BASE) as client:
            with pytest.raises(OutcomeUnknown) as exc:
                await client.start_execution("w", {"q": "hi"}, run_key="job:1", reuse="failed_only")
    assert engine.start.call_count == 1
    assert "find_execution" in str(exc.value)


async def test_find_execution_needs_the_echo() -> None:
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        async with JamjetClient(BASE) as client:
            assert await client.find_execution("job:1") is None
            await client.start_execution("w", {"q": "hi"}, run_key="job:1")
            found = await client.find_execution("job:1")
    assert found == {"execution_id": "exec_1", "status": "running"}
    assert engine.lookup.calls[0].request.url.params["run_key"] == "job:1"

    # An older engine ignores the query and lists every run.
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, commit="0a1b2c3", honours=False)
        async with JamjetClient(BASE) as client:
            await client.start_execution("w", {"q": "hi"})
            with pytest.raises(EngineFeatureMissing) as exc:
                await client.find_execution("job:1")
    assert exc.value.feature == "run_key"


async def test_a_key_with_a_control_character_is_refused_locally() -> None:
    bad = ["job:1\n", "job\x00:1", "job​:1", "job‮:1", "job 1", "", "k" * 201]
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        async with JamjetClient(BASE) as client:
            for key in bad:
                with pytest.raises(ValueError, match="run_key must be 1 to 200"):
                    await client.start_execution("w", {"q": "hi"}, run_key=key)
                with pytest.raises(ValueError, match="run_key must be 1 to 200"):
                    await client.find_execution(key)
            assert mock.calls.call_count == 0
            # The longest key the engine takes goes through.
            longest = "k" * 200
            started = await client.start_execution("w", {"q": "hi"}, run_key=longest)
    assert started["run_key"] == longest
    assert engine.started == ["exec_1"]


async def test_reuse_is_checked_locally() -> None:
    with respx.mock(assert_all_called=False) as mock:
        FakeEngine(mock, features=["run_key"])
        async with JamjetClient(BASE) as client:
            with pytest.raises(ValueError, match="reuse must be"):
                await client.start_execution("w", {}, run_key="job:1", reuse="always")  # type: ignore[arg-type]
            with pytest.raises(ValueError, match="reuse applies to a start with a run_key only"):
                await client.start_execution("w", {}, reuse="never")
        assert mock.calls.call_count == 0


def _completed(execution_id: str) -> dict[str, Any]:
    return {
        "execution_id": execution_id,
        "status": "completed",
        "current_state": {"messages": [], "last_model_output": "done", "last_model_finish_reason": "stop"},
    }


async def test_run_durable_with_a_key_resumes_the_same_run() -> None:
    with respx.mock(assert_all_called=False) as mock:
        engine = FakeEngine(mock, features=["run_key"])
        mock.get(f"{BASE}/executions/exec_1").respond(200, json=_completed("exec_1"))
        mock.get(f"{BASE}/executions/exec_1/events").respond(200, json={"events": []})
        first = await _agent().run_durable("hi", runtime_url=BASE, run_key="job:1")
        again = await _agent().run_durable("hi", runtime_url=BASE, run_key="job:1")
    assert first.execution_id == "exec_1"
    assert again.execution_id == "exec_1"
    assert engine.started == ["exec_1"]
    assert [json.loads(c.request.content)["run_key"] for c in engine.start.calls] == ["job:1", "job:1"]
