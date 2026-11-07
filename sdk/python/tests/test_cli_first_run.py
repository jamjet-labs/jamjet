"""Regression tests for the first-run path.

Each test pins one defect that a new user hit on the way from install to a
first run: ``jamjet validate``/``jamjet run`` on a ``.py`` workflow, ``jamjet
inspect``/``jamjet events`` on the event shape the runtime really sends, the
client dropping the server's error message, the default workflow version, and
coordinator weights the runtime refused.
"""

from __future__ import annotations

import json
import re
import textwrap
from pathlib import Path
from typing import Any

import httpx
import pytest
import respx
from typer.testing import CliRunner

from jamjet import Workflow
from jamjet.cli.main import app
from jamjet.client import JamjetClient
from jamjet.workflow.graph import WorkflowGraph
from jamjet.workflow.ir_compiler import compile_yaml

runner = CliRunner()

BASE = "http://fake:7700"
REPO_ROOT = Path(__file__).resolve().parents[3]

PY_WORKFLOW = """
from pydantic import BaseModel

from jamjet import Workflow

workflow = Workflow("py-first-run")


@workflow.state
class State(BaseModel):
    query: str
    answer: str | None = None


@workflow.step
async def think(state: State) -> State:
    return state


@workflow.step
async def reply(state: State) -> State:
    return state
"""


@pytest.fixture()
def py_workflow(tmp_path: Path) -> Path:
    path = tmp_path / "flow.py"
    path.write_text(textwrap.dedent(PY_WORKFLOW))
    return path


# ── validate / run on a .py workflow ─────────────────────────────────────────


def test_validate_accepts_a_python_workflow(py_workflow: Path) -> None:
    result = runner.invoke(app, ["validate", str(py_workflow)])
    assert result.exit_code == 0, result.output
    assert "Valid." in result.output
    assert "workflow_id=py-first-run" in result.output
    assert "Nodes: 2  Edges: 2" in result.output


def test_validate_output_prints_the_runtime_ir_for_a_python_workflow(py_workflow: Path) -> None:
    result = runner.invoke(app, ["validate", str(py_workflow), "--output"])
    assert result.exit_code == 0, result.output
    ir = json.loads(result.output[result.output.index("{") :])
    assert ir["nodes"]["think"]["kind"]["type"] == "python_fn"
    assert ir["start_node"] == "think"


def test_run_registers_the_runtime_ir_for_a_python_workflow(py_workflow: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    calls: dict[str, Any] = {}

    class FakeClient:
        async def __aenter__(self) -> FakeClient:
            return self

        async def __aexit__(self, *args: Any) -> None:
            return None

        async def create_workflow(self, ir: Any) -> dict[str, Any]:
            calls["ir"] = ir
            return {}

        async def start_execution(self, **kwargs: Any) -> dict[str, Any]:
            calls["start"] = kwargs
            return {"execution_id": "exec_1"}

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime, token=None: FakeClient())
    result = runner.invoke(app, ["run", str(py_workflow), "--no-follow", "--runtime", BASE])

    assert result.exit_code == 0, result.output
    ir = calls["ir"]
    # The body POST /workflows takes: a dict keyed by node id, each with a kind.
    assert isinstance(ir, dict)
    assert ir["workflow_id"] == "py-first-run"
    assert set(ir["nodes"]) == {"think", "reply"}
    assert ir["nodes"]["reply"]["kind"]["type"] == "python_fn"
    assert calls["start"] == {"workflow_id": "py-first-run", "input": {}, "workflow_version": "1.0.0"}


# ── inspect / events on runtime-shaped events ────────────────────────────────


def _event(seq: int, **kind: Any) -> dict[str, Any]:
    """An event as GET /executions/{id}/events returns it (runtime/state/src/event.rs)."""
    return {
        "id": f"00000000-0000-0000-0000-00000000000{seq}",
        "execution_id": "exec_1",
        "sequence": seq,
        "kind": kind,
        "created_at": "2026-11-07T10:00:0{seq}Z",
    }


RUNTIME_EVENTS = [
    _event(1, type="node_started", node_id="lookup"),
    _event(2, type="node_completed", node_id="lookup", output={}, duration_ms=5),
    _event(3, type="node_started", node_id="ask"),
    _event(
        4,
        type="agent_tool_invoked",
        node_id="ask",
        agent_uri="jamjet://agents/helper",
        mode="sync",
        protocol="a2a",
        input_hash="abc",
    ),
    _event(
        5,
        type="agent_tool_turn",
        node_id="ask",
        turn_number=1,
        direction="outbound",
        content_summary="hello",
        tokens=3,
        cost=0.0,
    ),
    _event(
        6,
        type="agent_tool_completed",
        node_id="ask",
        output={"answer": "hi"},
        provenance=None,
        total_cost=0.0,
        latency_ms=42,
        total_turns=1,
    ),
    _event(7, type="node_completed", node_id="ask", output={}, duration_ms=50),
]


@pytest.fixture()
def events_client(monkeypatch: pytest.MonkeyPatch) -> None:
    class FakeClient:
        async def __aenter__(self) -> FakeClient:
            return self

        async def __aexit__(self, *args: Any) -> None:
            return None

        async def get_execution(self, execution_id: str) -> dict[str, Any]:
            return {"execution_id": execution_id, "status": "completed"}

        async def get_events(self, execution_id: str) -> dict[str, Any]:
            return {"events": RUNTIME_EVENTS}

    monkeypatch.setattr("jamjet.cli.main._client", lambda runtime, token=None: FakeClient())


def test_inspect_reads_runtime_events(events_client: None) -> None:
    result = runner.invoke(app, ["inspect", "exec_1", "--runtime", BASE])
    assert result.exit_code == 0, result.output
    assert "Protocol Activity" in result.output
    assert "ask [a2a] — 3 events, 1 turns, 42ms" in result.output


def test_events_node_filter_reads_runtime_events(events_client: None) -> None:
    result = runner.invoke(app, ["events", "exec_1", "--node", "lookup", "--runtime", BASE])
    assert result.exit_code == 0, result.output
    assert "node_started" in result.output
    assert "node_completed" in result.output
    assert "ask" not in result.output


# ── the client surfaces the server's error message ───────────────────────────


@respx.mock
async def test_client_error_carries_the_json_error_message() -> None:
    respx.post(f"{BASE}/workflows").respond(400, json={"error": "workflow ir: missing field `capability_fit`"})
    async with JamjetClient(base_url=BASE) as c:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await c.create_workflow({"workflow_id": "w"})
    assert exc.value.response.status_code == 400
    assert "missing field `capability_fit`" in str(exc.value)
    assert getattr(exc.value, "detail", None) == "workflow ir: missing field `capability_fit`"


@respx.mock
async def test_client_error_carries_a_plain_text_body() -> None:
    # axum's JSON extractor rejects a bad body with a text/plain message.
    respx.post(f"{BASE}/executions").respond(422, text="Failed to deserialize the JSON body: missing field `input`")
    async with JamjetClient(base_url=BASE) as c:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await c.start_execution("w", {})
    assert "422" in str(exc.value)
    assert "missing field `input`" in str(exc.value)


@respx.mock
async def test_client_error_without_a_body_keeps_the_status() -> None:
    respx.get(f"{BASE}/executions/exec_9").respond(404)
    async with JamjetClient(base_url=BASE) as c:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await c.get_execution("exec_9")
    assert exc.value.response.status_code == 404
    assert "404" in str(exc.value)


# ── no command for a route the runtime does not serve ────────────────────────


def test_agents_trace_is_gone() -> None:
    # The runtime has no /agents/{id}/trace route; its SPA fallback answered
    # with an HTML page, so the command could never work.
    result = runner.invoke(app, ["agents", "trace", "agent-1", "--runtime", BASE])
    assert result.exit_code != 0
    assert "No such command" in result.output
    assert not hasattr(JamjetClient, "get_agent_trace")


# ── the default workflow version is the runtime's ────────────────────────────


_VERSION_DEFAULT = re.compile(r'workflow_version\.unwrap_or_else\(\|\| "([^"]+)"\.into\(\)\)')


def _runtime_default_version() -> str:
    """The version the runtime assumes when a run or a cron job names none.

    Both routes must agree, or a workflow registered with the SDK default can
    be started but not scheduled (or the other way round).
    """
    sources = {
        "start_run": REPO_ROOT / "runtime" / "api" / "src" / "routes.rs",
        "create_cron": REPO_ROOT / "runtime" / "api" / "src" / "cron.rs",
    }
    if not all(path.is_file() for path in sources.values()):
        pytest.skip("runtime sources not in this checkout")
    found = {}
    for route, path in sources.items():
        match = _VERSION_DEFAULT.search(path.read_text())
        assert match, f"could not find the default version in {route} ({path.relative_to(REPO_ROOT)})"
        found[route] = match.group(1)
    assert found["start_run"] == found["create_cron"], f"runtime defaults disagree: {found}"
    return found["start_run"]


def test_default_workflow_version_matches_the_runtime() -> None:
    expected = _runtime_default_version()
    assert Workflow("w").version == expected
    assert WorkflowGraph("w").version == expected
    assert compile_yaml("workflow:\n  id: w\nnodes:\n  a:\n    type: tool\n")["version"] == expected


# ── coordinator weights the runtime accepts ──────────────────────────────────


def _coordinator_kind(**kwargs: Any) -> dict[str, Any]:
    graph = WorkflowGraph("w")
    graph.add_coordinator("route", task="pick one", required_skills=["x"], **kwargs)
    return graph.compile()["nodes"]["route"]["kind"]


def test_coordinator_without_weights_leaves_them_to_the_runtime() -> None:
    assert "weights" not in _coordinator_kind()
    assert "weights" not in _coordinator_kind(weights={})


def test_coordinator_partial_weights_are_sent_complete() -> None:
    kind = _coordinator_kind(weights={"capability_fit": 3})
    assert kind["weights"] == {
        "capability_fit": 3.0,
        "cost_fit": 1.0,
        "latency_fit": 1.0,
        "trust_compatibility": 1.0,
        "historical_performance": 1.0,
    }


def test_coordinator_rejects_unknown_or_non_numeric_weights() -> None:
    with pytest.raises(ValueError, match="capabilty_fit"):
        _coordinator_kind(weights={"capabilty_fit": 2.0})
    with pytest.raises(ValueError, match="cost_fit"):
        _coordinator_kind(weights={"cost_fit": "high"})


def test_coordinator_weight_dimensions_match_the_runtime() -> None:
    from jamjet.workflow.nodes import COORDINATOR_WEIGHT_DIMENSIONS

    source = REPO_ROOT / "runtime" / "core" / "src" / "coordinator.rs"
    if not source.is_file():
        pytest.skip("runtime sources not in this checkout")
    body = re.search(r"pub struct DimensionWeights \{(.*?)\}", source.read_text(), re.S)
    assert body, "DimensionWeights not found in runtime/core/src/coordinator.rs"
    fields = re.findall(r"pub (\w+): f64", body.group(1))
    assert tuple(fields) == COORDINATOR_WEIGHT_DIMENSIONS


@respx.mock
async def test_create_cron_job_names_the_default_version() -> None:
    # An older runtime assumes 0.1.0 for POST /cron when the version is left out.
    route = respx.post(f"{BASE}/cron").respond(201, json={"name": "nightly"})
    async with JamjetClient(base_url=BASE) as c:
        await c.create_cron_job(name="nightly", cron_expression="0 2 * * *", workflow_id="w")
    body = json.loads(route.calls[0].request.content)
    assert body["workflow_version"] == "1.0.0"


# ── error text is capped, one line, and never read as markup ─────────────────


@respx.mock
async def test_client_error_detail_is_capped_and_escaped() -> None:
    echoed = "bad value \x1b[31mred\x1b[0m\nnext line " + "x" * 2000
    respx.post(f"{BASE}/workflows").respond(400, json={"error": echoed})
    async with JamjetClient(base_url=BASE) as c:
        with pytest.raises(httpx.HTTPStatusError) as exc:
            await c.create_workflow({"workflow_id": "w"})
    detail = getattr(exc.value, "detail", "")
    assert "\x1b" not in str(exc.value)
    assert "\\x1b[31m" in detail
    assert "\n" not in str(exc.value)
    assert len(detail) <= 503
    assert detail.endswith("...")


def test_cli_prints_error_text_literally(tmp_path: Path) -> None:
    path = tmp_path / "flow.py"
    path.write_text('raise ValueError("[bold red]not markup[/bold red] \\x1b[2J")\n')
    result = runner.invoke(app, ["validate", str(path)])
    assert result.exit_code == 1
    assert "[bold red]not markup[/bold red]" in result.output
    assert "\x1b[2J" not in result.output


async def test_worker_does_not_send_server_echoed_values_to_fail() -> None:
    from jamjet.cli.main import _worker_loop

    request = httpx.Request("POST", f"{BASE}/work-items/wi-1/complete")
    response = httpx.Response(400, request=request, json={"error": "invalid output: 'card=4111111111111111'"})

    class Client:
        def __init__(self) -> None:
            self.failures: list[str] = []

        async def claim_work_item(self, worker_id: str, queue_types: list[str], lease_secs: Any = None) -> dict:
            return {
                "id": "wi-1",
                "execution_id": "exec_1",
                "node_id": "n",
                "queue_type": "python_tool",
                "payload": {"module": "json", "function": "dumps", "input": {}},
                "lease_fence": 1,
            }

        async def complete_work_item(self, *args: Any, **kwargs: Any) -> None:
            from jamjet.client import _raise_for_status

            _raise_for_status(response)

        async def fail_work_item(self, item_id: str, error: str, lease_fence: int = 0) -> None:
            self.failures.append(error)

        async def heartbeat_work_item(self, *args: Any, **kwargs: Any) -> dict:
            return {}

    client = Client()
    await _worker_loop(client, "w", ["python_tool"], once=True)
    assert client.failures, "the worker should report the failure"
    assert "4111111111111111" not in client.failures[0]
    assert "HTTP 400" in client.failures[0]
