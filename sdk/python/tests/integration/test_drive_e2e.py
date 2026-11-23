"""Drive mode end to end, on a real engine.

Opt-in. Each test starts the engine named by JAMJET_SERVER_PATH on a free
loopback port, with its own SQLite file:

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_drive_e2e.py -q

Two tests need more, and skip without it:

- JAMJET_OLD_SERVER_PATH: a jamjet-server built from the python-sdk-v0.13.0 tag.
- JAMJET_OLD_SDK_PYTHON: the python of a venv with jamjet[sidecar]==0.13.0 installed.

test_drive_on_a_request_driven_engine needs an engine with request-driven
mode (JAMJET_BACKGROUND=off and POST /ops/tick) and JAMJET_E2E_REQUEST_DRIVEN=1.

No model call and no network. The caller's model is a router on a loopback
port that answers from a script and keeps each answer by the step's
idempotency key, as a router that bills per call would. The identity token
comes from a fake token source and is checked by a proxy on loopback.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from collections import Counter
from collections.abc import Callable, Coroutine, Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, TypeVar

import httpx
import pytest

from jamjet import Agent, tool
from jamjet.model.seam import Model
from jamjet.model.types import ModelRequest, ModelResponse

from .devstack import free_port, say

pytestmark = [
    pytest.mark.skipif(
        os.environ.get("JAMJET_E2E") != "1" or not os.environ.get("JAMJET_SERVER_PATH"),
        reason="opt-in: set JAMJET_E2E=1 and JAMJET_SERVER_PATH=<jamjet-server binary>",
    ),
    # The agent's own audit chain is not what these tests are about.
    pytest.mark.filterwarnings("ignore:JAMJET_AUDIT_SIGNING_KEY is not set:UserWarning"),
]

SDK_ROOT = Path(__file__).resolve().parents[2]
PROMPT = "What is the weather in Lisbon?"
ANSWER = "Lisbon is sunny today."
# The driven agent's nodes with max_turns=1: model, the tool gate, tools, the final model.
NODES = {"__model_0__", "__tool_gate_0__", "__tools_0__", "__model_1__"}
ROUTER_MODEL = "openai/router-test"
ANSI = re.compile(r"\x1b\[[0-9;]*m")
_T = TypeVar("_T")


# ── The agent, shared by the test process and the driver processes ────────────


@tool
def lookup_city(city: str) -> str:
    """The weather in a city."""
    return f"{city}: sunny, 24 C"


class RouterBackend:
    """A seam backend that sends each model step to the router over HTTP,
    with the step's idempotency key, and reports the usage the router gives."""

    def __init__(self, url: str) -> None:
        self.url = url

    async def complete(self, request: ModelRequest) -> ModelResponse:
        step = request.metadata["jamjet"]
        async with httpx.AsyncClient(timeout=120) as client:
            r = await client.post(
                f"{self.url}/complete",
                json={
                    "idempotency_key": step["idempotency_key"],
                    "node_id": step["node_id"],
                    "attempt": step["attempt"],
                    "model": request.ref.litellm_model,
                    "messages": request.messages,
                },
            )
        r.raise_for_status()
        answer = r.json()
        return ModelResponse(
            message={"content": answer.get("content"), "tool_calls": answer.get("tool_calls")},
            input_tokens=answer["input_tokens"],
            output_tokens=answer["output_tokens"],
            cost_usd=answer["cost_usd"],
            usage_reported=True,
            model=answer["model"],
        )

    def stream(self, request: ModelRequest) -> Any:
        raise NotImplementedError("drive mode does not stream")


def drive_agent() -> Agent:
    return Agent(
        "drive-e2e",
        model=ROUTER_MODEL,
        tools=[lookup_city],
        instructions="Answer with the tool.",
        strategy="react",
    )


async def drive_once(
    engine_url: str,
    router_url: str,
    *,
    run_key: str | None,
    lease_secs: int | None = None,
    auth: httpx.Auth | None = None,
) -> dict[str, Any]:
    result = await drive_agent().run_durable(
        PROMPT,
        max_turns=1,
        runtime_url=engine_url,
        auth=auth,
        run_key=run_key,
        drive=True,
        model=Model(backend=RouterBackend(router_url), ungoverned=True),
        lease_secs=lease_secs,
    )
    return {"execution_id": result.execution_id, "output": result.output}


def run_within(seconds: float, work: Coroutine[Any, Any, _T]) -> _T:
    """``work`` run to its end, or a failure after ``seconds``: a drive that
    never ends (an older engine answers every claim with no reason) fails the
    test instead of holding it."""

    async def bounded() -> _T:
        try:
            return await asyncio.wait_for(work, seconds)
        except TimeoutError:
            pytest.fail(f"the call did not return within {seconds:g} s")

    return asyncio.run(bounded())


def driver_main() -> None:
    """A driver process: ``python -c 'from <this module> import driver_main; driver_main()' <json>``."""
    args = json.loads(sys.argv[1])
    done = asyncio.run(
        drive_once(args["engine"], args["router"], run_key=args["run_key"], lease_secs=args.get("lease_secs"))
    )
    print("RESULT " + json.dumps(done), flush=True)


# ── Processes and servers on loopback ─────────────────────────────────────────


def clean_env(**extra: str) -> dict[str, str]:
    """This process's environment less model keys, JamJet settings and database URLs."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not k.startswith("JAMJET_")
        and k not in ("DATABASE_URL", "GOOGLE_APPLICATION_CREDENTIALS")
    }
    env.update(PYTHONUNBUFFERED="1", NO_COLOR="1", **extra)
    return env


def wait_until(check: Callable[[], bool], timeout: float, what: str, log: Callable[[], str] = lambda: "") -> None:
    deadline = time.monotonic() + timeout
    while not check():
        if time.monotonic() > deadline:
            pytest.fail(f"timed out after {timeout:g} s waiting for {what}\n{log()[-4000:]}")
        time.sleep(0.1)


class Engine:
    """A jamjet-server in dev mode on a free loopback port, with its own SQLite file."""

    def __init__(self, binary: str, root: Path, **env: str) -> None:
        root.mkdir(parents=True, exist_ok=True)
        self.binary = os.path.realpath(binary)
        self.root = root
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.log = root / "engine.log"
        self.env = env
        self.proc: subprocess.Popen[bytes] | None = None

    def start(self) -> Engine:
        env = clean_env(
            PORT=str(self.port),
            JAMJET_PORT=str(self.port),
            JAMJET_DEV_MODE="1",
            RUST_LOG="info",
            DATABASE_URL=f"sqlite://{self.root / 'runtime.db'}",
            **self.env,
        )
        with self.log.open("wb") as out:
            self.proc = subprocess.Popen(
                [self.binary], cwd=self.root, env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True
            )
        wait_until(self._healthy, 60, f"the engine on {self.url}", self.log_text)
        return self

    def _healthy(self) -> bool:
        assert self.proc is not None
        if self.proc.poll() is not None:
            pytest.fail(f"the engine exited {self.proc.returncode}:\n{self.log_text()[-4000:]}")
        try:
            return httpx.get(f"{self.url}/health", timeout=2).json().get("status") == "ok"
        except (httpx.HTTPError, ValueError):
            return False

    def stop(self) -> None:
        stop_process(self.proc)

    def log_text(self) -> str:
        return ANSI.sub("", self.log.read_text(errors="replace")) if self.log.exists() else ""

    def get(self, path: str, **params: Any) -> Any:
        r = httpx.get(f"{self.url}{path}", params=params, timeout=10)
        r.raise_for_status()
        return r.json()

    def node_completions(self, execution_id: str) -> Counter[str]:
        events = self.get(f"/executions/{execution_id}/events")["events"]
        return Counter(e["kind"]["node_id"] for e in events if e["kind"]["type"] == "node_completed")


def stop_process(proc: subprocess.Popen[Any] | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    with contextlib.suppress(OSError):
        os.killpg(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(OSError):
            os.killpg(proc.pid, signal.SIGKILL)
        proc.wait(timeout=10)


class _QuietServer(ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request: Any, client_address: Any) -> None:
        if isinstance(sys.exc_info()[1], ConnectionError):
            return  # a caller that went away: a killed driver
        super().handle_error(request, client_address)


class _Server:
    """A threaded HTTP server on a free loopback port, stopped by ``close``."""

    def __init__(self, handler: type[BaseHTTPRequestHandler]) -> None:
        self.httpd = _QuietServer(("127.0.0.1", 0), handler)
        self.httpd.owner = self  # type: ignore[attr-defined]
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002 - the base class's name
        pass

    def body(self) -> bytes:
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def answer(self, status: int, body: bytes, content_type: str, headers: dict[str, str] | None = None) -> None:
        try:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            for k, v in (headers or {}).items():
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the caller is gone: a killed driver


# The router's script: turn N answers a conversation with N assistant messages.
SCRIPT: list[dict[str, Any]] = [
    {"tool_calls": [{"id": "call_0", "name": "lookup_city", "arguments": {"city": "Lisbon"}}]},
    {"content": ANSWER},
]


class Router(_Server):
    """The caller's model router. It charges once per idempotency key and
    keeps the answer, so a step sent again gets the same answer for free.

    ``hold_first`` holds the first request until ``release`` is set.
    ``delay_s`` is added to every answer.
    """

    def __init__(self, *, hold_first: bool = False, delay_s: float = 0.0) -> None:
        self.lock = threading.Lock()
        self.seen: list[dict[str, Any]] = []  # every request: idempotency_key, node_id, attempt
        self.charges: Counter[str] = Counter()
        self.answers: dict[str, dict[str, Any]] = {}
        self.hold_first = hold_first
        self.delay_s = delay_s
        self.holding = threading.Event()
        self.release = threading.Event()
        super().__init__(_RouterHandler)

    def close(self) -> None:
        self.release.set()
        super().close()

    def respond(self, request: dict[str, Any]) -> dict[str, Any]:
        key = request["idempotency_key"]
        assert isinstance(key, str) and key, f"a model step came with no idempotency key: {request}"
        with self.lock:
            self.seen.append({k: request.get(k) for k in ("idempotency_key", "node_id", "attempt")})
            hold = self.hold_first and len(self.seen) == 1
            if key not in self.answers:
                turn = sum(1 for m in request["messages"] if m.get("role") == "assistant")
                self.charges[key] += 1
                self.answers[key] = {
                    **SCRIPT[turn],
                    "model": request["model"],
                    "input_tokens": 120,
                    "output_tokens": 30,
                    "cost_usd": 0.002,
                }
            answer = self.answers[key]
        if hold:
            self.holding.set()
            self.release.wait(120)
        time.sleep(self.delay_s)
        return answer


class _RouterHandler(_Handler):
    def do_POST(self) -> None:  # noqa: N802 - the base class's name
        router: Router = self.server.owner  # type: ignore[attr-defined]
        answer = router.respond(json.loads(self.body()))
        self.answer(200, json.dumps(answer).encode(), "application/json")


HOP_HEADERS = {"host", "content-length", "connection", "keep-alive", "transfer-encoding"}
Gate = Callable[[str, str, dict[str, str]], tuple[int, str, bytes] | None]


class Proxy(_Server):
    """Forwards every request to ``upstream`` and records it with its answer.

    ``gate(method, path, headers)`` may answer a request itself, as a platform
    in front of the engine does; it is then not forwarded.
    ``drop_headers`` are not passed on.
    """

    def __init__(self, upstream: str, *, gate: Gate | None = None, drop_headers: tuple[str, ...] = ()) -> None:
        self.upstream = httpx.Client(base_url=upstream, timeout=60)
        self.gate = gate
        self.drop = {h.lower() for h in drop_headers}
        self.lock = threading.Lock()
        self.records: list[dict[str, Any]] = []
        super().__init__(_ProxyHandler)

    def close(self) -> None:
        super().close()
        self.upstream.close()

    def calls(self) -> list[tuple[str, str]]:
        with self.lock:
            return [(r["method"], r["path"]) for r in self.records]


class _ProxyHandler(_Handler):
    def _forward(self) -> None:
        proxy: Proxy = self.server.owner  # type: ignore[attr-defined]
        body = self.body()
        headers = {k.lower(): v for k, v in self.headers.items()}
        record: dict[str, Any] = {"method": self.command, "path": self.path, "headers": headers}
        try:
            record["body"] = json.loads(body) if body else None
        except ValueError:
            record["body"] = None
        own = proxy.gate(self.command, self.path, headers) if proxy.gate else None
        if own is not None:
            status, content_type, payload = own
            record.update(status=status, answer=None, forwarded=False)
            with proxy.lock:
                proxy.records.append(record)
            self.answer(status, payload, content_type)
            return
        sent = {k: v for k, v in headers.items() if k not in HOP_HEADERS and k not in proxy.drop}
        r = proxy.upstream.request(self.command, self.path, content=body, headers=sent)
        try:
            answer = r.json()
        except ValueError:
            answer = None
        record.update(status=r.status_code, answer=answer, forwarded=True)
        with proxy.lock:
            proxy.records.append(record)
        self.answer(r.status_code, r.content, r.headers.get("content-type", "application/octet-stream"))

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = _forward  # noqa: N815 - the base class's names


class Driver:
    """A driver process: this module's ``driver_main`` under this interpreter."""

    def __init__(self, root: Path, name: str, **args: Any) -> None:
        self.out = root / f"{name}.out"
        code = f"from {__name__} import driver_main; driver_main()"
        with self.out.open("wb") as out:
            self.proc = subprocess.Popen(
                [sys.executable, "-c", code, json.dumps(args)],
                cwd=SDK_ROOT,
                env=clean_env(),
                stdout=out,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )

    def holding(self, router: Router) -> Callable[[], bool]:
        """A check for ``wait_until``: the router holds this driver's step. Fails if the driver exited."""

        def check() -> bool:
            if self.proc.poll() is not None:
                pytest.fail(f"the driver exited {self.proc.returncode}:\n{self.text()[-4000:]}")
            return router.holding.is_set()

        return check

    def kill(self) -> None:
        os.killpg(self.proc.pid, signal.SIGKILL)
        self.proc.wait(timeout=10)

    def result(self, timeout: float = 180) -> dict[str, Any]:
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            stop_process(self.proc)
            pytest.fail(f"the driver did not finish in {timeout:g} s:\n{self.text()[-4000:]}")
        text = self.text()
        found = [ln for ln in text.splitlines() if ln.startswith("RESULT ")]
        assert self.proc.returncode == 0 and found, f"the driver exited {self.proc.returncode}:\n{text[-4000:]}"
        return json.loads(found[-1][len("RESULT ") :])

    def text(self) -> str:
        return self.out.read_text(errors="replace")


@pytest.fixture
def servers() -> Iterator[list[Any]]:
    """Everything a test starts, stopped at its end whatever happens."""
    started: list[Any] = []
    yield started
    for thing in reversed(started):
        if isinstance(thing, Driver):
            stop_process(thing.proc)
        elif isinstance(thing, Engine):
            thing.stop()
        else:
            thing.close()


@pytest.fixture
def engine(tmp_path: Path, servers: list[Any]) -> Engine:
    e = Engine(os.environ["JAMJET_SERVER_PATH"], tmp_path / "engine")
    servers.append(e)
    return e.start()


def exec_id(raw: str) -> str:
    """A run id as a start answers it, ``exec_<32 hex>``; a listing gives the bare UUID."""
    return raw if raw.startswith("exec_") else "exec_" + raw.replace("-", "")


def assert_each_node_completed_once(engine: Engine, execution_id: str) -> None:
    assert engine.get(f"/executions/{execution_id}")["status"] == "completed"
    assert engine.node_completions(execution_id) == Counter({n: 1 for n in NODES})


# ── The tests ─────────────────────────────────────────────────────────────────


def test_a_killed_driver_resumes_by_run_key_and_each_node_completes_once(
    engine: Engine, tmp_path: Path, servers: list[Any]
) -> None:
    router = Router(hold_first=True)
    servers.append(router)
    args = {"engine": engine.url, "router": router.url, "run_key": "drive-e2e:killed", "lease_secs": 5}

    first = Driver(tmp_path, "driver-1", **args)
    servers.append(first)
    wait_until(first.holding(router), 90, "the first model step to reach the router", first.text)
    found = engine.get("/executions", run_key="drive-e2e:killed")["executions"]
    assert len(found) == 1, found
    run_id = exec_id(found[0]["execution_id"])
    first.kill()
    assert first.proc.returncode == -signal.SIGKILL
    router.release.set()

    second = Driver(tmp_path, "driver-2", **args)
    servers.append(second)
    done = second.result()

    assert done == {"execution_id": run_id, "output": ANSWER}
    assert len(engine.get("/executions")["executions"]) == 1
    assert_each_node_completed_once(engine, run_id)
    held_key = router.seen[0]["idempotency_key"]
    assert [s["idempotency_key"] for s in router.seen].count(held_key) == 2, router.seen
    assert [s["node_id"] for s in router.seen] == ["__model_0__", "__model_0__", "__model_1__"], router.seen
    assert router.charges == Counter({held_key: 1, router.seen[2]["idempotency_key"]: 1}), router.charges
    say("killed driver", f"run {run_id}\nrouter saw {router.seen}\ncharged {dict(router.charges)}")


def test_two_drivers_on_one_run_complete_each_node_once(engine: Engine, tmp_path: Path, servers: list[Any]) -> None:
    # Every answer takes 2 s, so the second driver asks while the first holds a step.
    router = Router(delay_s=2.0)
    proxy = Proxy(engine.url)
    servers += [router, proxy]
    args = {"engine": proxy.url, "router": router.url, "run_key": "drive-e2e:two"}

    a = Driver(tmp_path, "driver-a", **args)
    b = Driver(tmp_path, "driver-b", **args)
    servers += [a, b]
    done_a, done_b = a.result(), b.result()

    assert done_a["execution_id"] == done_b["execution_id"]
    assert done_a["output"] == done_b["output"] == ANSWER
    run_id = done_a["execution_id"]
    assert len(engine.get("/executions")["executions"]) == 1
    assert_each_node_completed_once(engine, run_id)
    assert [s["node_id"] for s in router.seen] == ["__model_0__", "__model_1__"], router.seen
    assert set(router.charges.values()) == {1} and len(router.charges) == 2, router.charges

    # The race opened: both drivers claimed on the run, and one was told the
    # other held its step.
    claims = [r for r in proxy.records if r["path"] == "/work-items/claim" and r["status"] == 200]
    assert {c["body"]["execution_id"] for c in claims} == {run_id}
    assert len({c["body"]["worker_id"] for c in claims}) == 2, claims
    reasons = Counter(c["answer"].get("reason") for c in claims if not c["answer"].get("claimed"))
    assert reasons["claimed_by_other"] >= 1, reasons
    starts = [r["status"] for r in proxy.records if r["method"] == "POST" and r["path"] == "/executions"]
    assert sorted(starts) == [200, 201], starts
    by_worker = Counter(c["body"]["worker_id"] for c in claims)
    say("two drivers", f"run {run_id}\nclaims per driver {dict(by_worker)}\nrefusals {dict(reasons)}")


# The python_fn step of the approval workflow below, run by the driver in this process.
PREPARED: list[Any] = []


def prepare_notes(state: dict[str, Any]) -> dict[str, Any]:
    PREPARED.append(state.get("release"))
    return {"notes": f"release {state.get('release')} notes written"}


def approval_workflow() -> dict[str, Any]:
    """A python_fn step the driver claims, then a human_approval node, then the end."""

    def node(node_id: str, kind: dict[str, Any]) -> dict[str, Any]:
        return {
            "id": node_id,
            "kind": kind,
            "retry_policy": None,
            "node_timeout_secs": None,
            "description": None,
            "labels": {},
        }

    return {
        "workflow_id": "drive-e2e-approval",
        "version": "0.1.0",
        "name": None,
        "description": None,
        "state_schema": "",
        "start_node": "prepare",
        "nodes": {
            "prepare": node(
                "prepare",
                {
                    "type": "python_fn",
                    "module": __name__,
                    "function": "prepare_notes",
                    "output_schema": "",
                    "claim": "by_execution",
                },
            ),
            "review": node(
                "review",
                {
                    "type": "human_approval",
                    "description": "Publish the notes?",
                    "timeout_secs": None,
                    "fallback_node": None,
                },
            ),
        },
        "edges": [
            {"from": "prepare", "to": "review", "condition": None},
            {"from": "review", "to": "end", "condition": None},
        ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {},
    }


def test_a_drive_stops_at_an_approval_node_at_once_and_a_second_drive_finishes(engine: Engine) -> None:
    from jamjet.client import JamjetClient
    from jamjet.drive import DRIVE_QUEUES, RunWaiting, drive_run

    stall_after_s = 60.0
    PREPARED.clear()

    async def scenario() -> tuple[str, RunWaiting, float, dict[str, Any], dict[str, Any]]:
        async with JamjetClient(engine.url) as client:
            ir = approval_workflow()
            await client.create_workflow(ir)
            started = await client.start_execution(ir["workflow_id"], {"release": "1.4"}, ir["version"])
            run_id = started["execution_id"]
            began = time.monotonic()
            with pytest.raises(RunWaiting) as waiting:
                await drive_run(
                    client, run_id, queues=list(DRIVE_QUEUES), model=None, run_key=None, stall_after_s=stall_after_s
                )
            waited_s = time.monotonic() - began
            (pending,) = (await client.list_approvals(run_id))["pending"]
            decision = await client.approve(
                run_id,
                "approved",
                node_id=pending["node_id"],
                action_digest=(pending.get("context") or {}).get("action_digest"),
                user_id="reviewer",
            )
            done = await drive_run(client, run_id, queues=list(DRIVE_QUEUES), model=None, run_key=None)
            return run_id, waiting.value, waited_s, decision, done

    run_id, waiting, waited_s, decision, done = run_within(150, scenario())

    # The step ran here, then the drive left at the approval, well before the stall bound.
    assert PREPARED == ["1.4"]
    assert (waiting.execution_id, waiting.waiting_for, waiting.node_id) == (run_id, "approval", "review")
    assert waited_s < stall_after_s / 2, f"the drive took {waited_s:.1f} s to say the run waits"
    assert decision["accepted"] is True, decision
    # The second drive saw the run through to its end and ran nothing again.
    assert done["status"] == "completed", done
    assert done["current_state"]["notes"] == "release 1.4 notes written"
    assert PREPARED == ["1.4"]
    assert engine.node_completions(run_id)["prepare"] == 1
    say("approval node", f"run {run_id}: waiting for approval at {waiting.node_id} after {waited_s:.1f} s")


@pytest.mark.skipif(
    not os.environ.get("JAMJET_OLD_SERVER_PATH"),
    reason="set JAMJET_OLD_SERVER_PATH=<jamjet-server built from python-sdk-v0.13.0>",
)
def test_the_new_sdk_against_the_0_13_engine_refuses_before_writing(tmp_path: Path, servers: list[Any]) -> None:
    from jamjet.transport import EngineFeatureMissing

    old = Engine(os.environ["JAMJET_OLD_SERVER_PATH"], tmp_path / "old-engine")
    servers.append(old)
    old.start()
    assert "features" not in old.get("/health"), "this engine lists features: it is not the 0.13 engine"
    proxy = Proxy(old.url)
    router = Router()
    servers += [proxy, router]

    with pytest.raises(EngineFeatureMissing) as drive:
        run_within(30, drive_once(proxy.url, router.url, run_key=None))
    assert drive.value.feature == "python_fn_by_execution"
    with pytest.raises(EngineFeatureMissing) as keyed:
        run_within(30, drive_agent().run_durable(PROMPT, max_turns=1, runtime_url=proxy.url, run_key="drive-e2e:old"))
    assert keyed.value.feature == "run_key"
    with pytest.raises(EngineFeatureMissing) as both:
        run_within(30, drive_once(proxy.url, router.url, run_key="drive-e2e:old"))
    assert both.value.feature == "python_fn_by_execution"

    # Each call read /health and sent nothing else.
    assert proxy.calls() == [("GET", "/health")] * 3
    assert router.seen == []
    assert old.get("/executions")["executions"] == []
    log = old.log_text()
    assert "listening on" in log, log[-2000:]
    assert "drive-e2e" not in log and "exec_" not in log, log[-4000:]


OLD_TOOLS = '''\
from jamjet import tool


@tool
def lookup_city(city: str) -> str:
    """The weather in a city."""
    with open("tool-calls.log", "a") as log:
        log.write(city + "\\n")
    return f"{city}: sunny, 24 C"
'''

OLD_SCRIPT = """\
turns:
  - tool_calls:
      - name: lookup_city
        arguments: {city: Lisbon}
  - content: "Lisbon is sunny today."
"""

OLD_RUN = """\
import asyncio, json, sys

import jamjet
from jamjet import Agent
from old_tools import lookup_city

assert jamjet.__version__ == "0.13.0", jamjet.__version__
agent = Agent("old-sdk", model="scripted/script.yaml", tools=[lookup_city], instructions="Answer with the tool.",
              strategy="react")
result = asyncio.run(agent.run_durable("What is the weather in Lisbon?", max_turns=1, runtime_url=sys.argv[1]))
print("RESULT " + json.dumps({"execution_id": result.execution_id, "output": result.output,
                              "tool_calls": result.tool_calls}, default=str), flush=True)
"""


@pytest.mark.skipif(
    not os.environ.get("JAMJET_OLD_SDK_PYTHON"),
    reason="set JAMJET_OLD_SDK_PYTHON=<python of a venv with jamjet[sidecar]==0.13.0>",
)
def test_the_0_13_sdk_runs_its_agent_on_the_new_engine(tmp_path: Path, servers: list[Any]) -> None:
    old_python = os.environ["JAMJET_OLD_SDK_PYTHON"]
    project = tmp_path / "old-sdk"
    (project / "home").mkdir(parents=True)
    (project / "old_tools.py").write_text(OLD_TOOLS)
    (project / "script.yaml").write_text(OLD_SCRIPT)
    (project / "run_old.py").write_text(OLD_RUN)
    env = clean_env(HOME=str(project / "home"), JAMJET_SERVER_PATH=os.path.realpath(os.environ["JAMJET_SERVER_PATH"]))
    port, sidecar_port = free_port(), free_port()
    url = f"http://127.0.0.1:{port}"

    # The 0.13 `jamjet dev`: the model sidecar, the new engine, and the 0.13 `jamjet worker`.
    dev_log = project / "jamjet-dev.log"
    argv = ["-m", "jamjet", "dev", "--modules", "old_tools", "--port", str(port), "--sidecar-port", str(sidecar_port)]
    with dev_log.open("wb") as out:
        dev = subprocess.Popen(
            [old_python, *argv], cwd=project, env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True
        )
    servers.append(_Stoppable(dev))

    def ready() -> bool:
        if dev.poll() is not None:
            pytest.fail(f"the 0.13 jamjet dev exited {dev.returncode}:\n{dev_log.read_text(errors='replace')[-4000:]}")
        return "worker ready." in dev_log.read_text(errors="replace")

    wait_until(ready, 120, "the 0.13 jamjet dev stack", lambda: dev_log.read_text(errors="replace"))
    health = httpx.get(f"{url}/health", timeout=5).json()
    assert "run_key" in health["features"], "jamjet dev is not running the new engine"

    run = subprocess.run(
        [old_python, "run_old.py", url], cwd=project, env=env, capture_output=True, text=True, timeout=180
    )
    found = [ln for ln in run.stdout.splitlines() if ln.startswith("RESULT ")]
    assert run.returncode == 0 and found, f"{run.stdout}\n{run.stderr}\n{dev_log.read_text()[-4000:]}"
    done = json.loads(found[-1][len("RESULT ") :])

    assert done["output"] == ANSWER
    assert [(c["tool"], c["input"], c["output"]) for c in done["tool_calls"]] == [
        ("lookup_city", {"city": "Lisbon"}, "Lisbon: sunny, 24 C")
    ]
    assert (project / "tool-calls.log").read_text() == "Lisbon\n"  # run once, by the 0.13 worker
    execution = httpx.get(f"{url}/executions/{done['execution_id']}", timeout=5).json()
    assert execution["status"] == "completed"
    events = httpx.get(f"{url}/executions/{done['execution_id']}/events", timeout=5).json()["events"]
    completed = Counter(e["kind"]["node_id"] for e in events if e["kind"]["type"] == "node_completed")
    assert completed == Counter({n: 1 for n in NODES})


class _Stoppable:
    def __init__(self, proc: subprocess.Popen[Any]) -> None:
        self.proc = proc

    def close(self) -> None:
        stop_process(self.proc)


@pytest.mark.skipif(os.environ.get("JAMJET_E2E_REQUEST_DRIVEN") != "1", reason="needs request-driven mode")
def test_drive_on_a_request_driven_engine(tmp_path: Path, servers: list[Any]) -> None:
    e = Engine(os.environ["JAMJET_SERVER_PATH"], tmp_path / "engine", JAMJET_BACKGROUND="off", JAMJET_EMBED_CRON="0")
    servers.append(e)
    e.start()
    assert "ops_tick" in e.get("/health")["features"]
    assert "background loops are off" in e.log_text()
    router = Router()
    servers.append(router)

    ticks: list[dict[str, Any]] = []
    stop = threading.Event()

    def ticker() -> None:
        with httpx.Client(base_url=e.url, timeout=30) as client:
            while not stop.wait(0.2):
                r = client.post("/ops/tick", json={})
                r.raise_for_status()
                ticks.append(r.json())

    thread = threading.Thread(target=ticker, daemon=True)
    thread.start()
    try:
        done = run_within(180, drive_once(e.url, router.url, run_key="drive-e2e:ticked"))
    finally:
        stop.set()
        thread.join(timeout=30)

    assert done["output"] == ANSWER
    assert_each_node_completed_once(e, done["execution_id"])
    assert ticks and any(t.get("ran") for t in ticks), ticks
    assert set(router.charges.values()) == {1} and len(router.charges) == 2, router.charges


def _fake_token(n: int) -> str:
    def part(value: dict[str, Any]) -> str:
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")

    now = int(time.time())
    return f"{part({'alg': 'none', 'typ': 'JWT'})}.{part({'iat': now, 'exp': now + 3600, 'n': n})}.c2ln"


UNAUTHORIZED_PAGE = b"<html><head><title>401 Unauthorized</title></head><body>Unauthorized</body></html>"


def test_every_request_carries_the_identity_token(engine: Engine, servers: list[Any]) -> None:
    from jamjet.auth import TokenSourceAuth

    tokens: list[str] = []

    def fetch() -> str:
        tokens.append(_fake_token(len(tokens) + 1))
        return tokens[-1]

    refused: list[tuple[str, str]] = []  # the one 401 the platform gives on purpose
    unexpected: list[tuple[str, str, str | None]] = []  # a request with no token or a stale one

    def platform(method: str, path: str, headers: dict[str, str]) -> tuple[int, str, bytes] | None:
        sent = headers.get("x-serverless-authorization")
        if not tokens or sent != f"Bearer {tokens[-1]}":
            unexpected.append((method, path, sent))
            return 401, "text/html; charset=utf-8", UNAUTHORIZED_PAGE
        if path == "/work-items/claim" and not refused:
            refused.append((method, path))
            return 401, "text/html; charset=utf-8", UNAUTHORIZED_PAGE
        return None

    proxy = Proxy(engine.url, gate=platform, drop_headers=("x-serverless-authorization",))
    router = Router()
    servers += [proxy, router]
    auth = TokenSourceAuth(fetch, origin=proxy.url)

    done = run_within(180, drive_once(proxy.url, router.url, run_key="drive-e2e:token", auth=auth))

    assert done["output"] == ANSWER
    assert_each_node_completed_once(engine, done["execution_id"])
    assert unexpected == []
    assert refused == [("POST", "/work-items/claim")]
    assert len(tokens) == 2  # the first, and one fresh after the 401
    calls = proxy.calls()
    assert ("GET", "/health") in calls and ("POST", "/executions") in calls
    assert sum(1 for c in calls if c == ("POST", "/work-items/claim")) >= 4
    # Every request after the refusal carried the fresh token; the refused claim was sent again.
    after = proxy.records[[r["forwarded"] for r in proxy.records].index(False) + 1 :]
    assert after and all(r["headers"]["x-serverless-authorization"] == f"Bearer {tokens[1]}" for r in after)
    assert (after[0]["method"], after[0]["path"]) == ("POST", "/work-items/claim")
    say("identity token", f"{len(calls)} requests, {len(tokens)} tokens fetched, refused {refused}")
