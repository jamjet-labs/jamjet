"""A Python worker and an agent's run_durable on an engine with its background loops off.

Opt-in. The test starts `jamjet dev` in a temp project with JAMJET_BACKGROUND=off,
so the engine moves a run only right after a write and when it is ticked
(POST /ops/tick); a thread in the test ticks it every 200 ms:

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_request_driven_e2e.py -q

The stack is the dev one: the model sidecar, the engine and `jamjet worker`.
No model call and no network: the model answers from a script.
"""

from __future__ import annotations

import contextlib
import json
import os
import signal
import subprocess
import sys
import threading
import time
from collections import Counter
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import httpx
import pytest

from .devstack import free_port, processes, say

pytestmark = pytest.mark.skipif(
    os.environ.get("JAMJET_E2E") != "1" or not os.environ.get("JAMJET_SERVER_PATH"),
    reason="opt-in: set JAMJET_E2E=1 and JAMJET_SERVER_PATH=<jamjet-server binary>",
)

ANSWER = "Lisbon is sunny today."
# The agent's nodes with max_turns=1: model, the tool gate, tools, the final model.
NODES = {"__model_0__", "__tool_gate_0__", "__tools_0__", "__model_1__"}

TOOLS = '''\
from jamjet import tool


@tool
def lookup_city(city: str) -> str:
    """The weather in a city."""
    with open("tool-calls.log", "a") as log:
        log.write(city + "\\n")
    return f"{city}: sunny, 24 C"
'''

SCRIPT = """\
turns:
  - tool_calls:
      - name: lookup_city
        arguments: {city: Lisbon}
  - content: "Lisbon is sunny today."
"""

RUN = """\
import asyncio, json, sys

from jamjet import Agent
from rd_tools import lookup_city

agent = Agent("rd-e2e", model="scripted/script.yaml", tools=[lookup_city], instructions="Answer with the tool.",
              strategy="react")
result = asyncio.run(agent.run_durable("What is the weather in Lisbon?", max_turns=1, runtime_url=sys.argv[1]))
print("RESULT " + json.dumps({"execution_id": result.execution_id, "output": result.output}), flush=True)
"""


def child_env(project: Path) -> dict[str, str]:
    """This process's environment less model keys, database URLs and JamJet
    settings, with a fresh HOME, the engine binary and the loops off."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not k.startswith("JAMJET_")
        and k not in ("DATABASE_URL", "GOOGLE_APPLICATION_CREDENTIALS")
    }
    env.update(
        HOME=str(project / "home"),
        PYTHONUNBUFFERED="1",
        NO_COLOR="1",
        # jamjet dev wraps what it prints at the terminal's width.
        COLUMNS="200",
        JAMJET_SERVER_PATH=os.path.realpath(os.environ["JAMJET_SERVER_PATH"]),
        JAMJET_BACKGROUND="off",
    )
    return env


class Stack:
    """`jamjet dev --modules rd_tools` in ``project`` on free ports."""

    def __init__(self, project: Path, env: dict[str, str]) -> None:
        self.project = project
        self.env = env
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.log = project / "jamjet-dev.log"
        self.children: dict[int, str] = {}
        argv = ["-m", "jamjet", "dev", "--modules", "rd_tools", "--port", str(self.port)]
        argv += ["--sidecar-port", str(free_port())]
        with self.log.open("wb") as out:
            self.proc = subprocess.Popen(
                [sys.executable, *argv],
                cwd=project,
                env=env,
                stdout=out,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )

    def text(self) -> str:
        return self.log.read_text(errors="replace") if self.log.exists() else ""

    def wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while "worker ready." not in self.text():
            # Note each child as it appears, so stop() can reach it if the start fails.
            self.children.update({pid: cmd for pid, ppid, cmd in processes() if ppid == self.proc.pid})
            if self.proc.poll() is not None or time.monotonic() > deadline:
                pytest.fail(f"jamjet dev did not start:\n{self.text()[-4000:]}")
            time.sleep(0.25)
        self.children.update({pid: cmd for pid, ppid, cmd in processes() if ppid == self.proc.pid})

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)  # jamjet dev stops its children
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=10)
        alive = {pid: cmd for pid, _, cmd in processes()}
        for pid, command in self.children.items():
            if alive.get(pid) == command:
                with contextlib.suppress(OSError):
                    os.killpg(pid, signal.SIGKILL)


@pytest.fixture
def stack(tmp_path: Path) -> Iterator[Stack]:
    project = tmp_path / "rd"
    (project / "home").mkdir(parents=True)
    (project / "rd_tools.py").write_text(TOOLS)
    (project / "script.yaml").write_text(SCRIPT)
    (project / "run_rd.py").write_text(RUN)
    s = Stack(project, child_env(project))
    try:
        s.wait_ready(120)
        yield s
    finally:
        s.stop()


def test_a_python_worker_drives_a_request_driven_engine(stack: Stack) -> None:
    health = httpx.get(f"{stack.url}/health", timeout=5).json()
    assert health["background"] == "off", health
    assert "ops_tick" in health["features"], health
    assert "background loops are off" in stack.text()

    # The scheduler's part: a tick every 200 ms. Dev mode takes it without a token.
    ticks: list[dict[str, Any]] = []
    failed: list[str] = []
    stop = threading.Event()

    def ticker() -> None:
        with httpx.Client(base_url=stack.url, timeout=30) as client:
            while not stop.wait(0.2):
                r = client.post("/ops/tick", json={})
                if r.status_code == 200:
                    ticks.append(r.json())
                else:
                    failed.append(f"{r.status_code} {r.text}")

    thread = threading.Thread(target=ticker, daemon=True)
    thread.start()
    started = time.monotonic()
    try:
        run = subprocess.run(
            [sys.executable, "run_rd.py", stack.url],
            cwd=stack.project,
            env=stack.env,
            capture_output=True,
            text=True,
            timeout=60,
        )
    finally:
        stop.set()
        thread.join(timeout=30)
    took = time.monotonic() - started

    found = [ln for ln in run.stdout.splitlines() if ln.startswith("RESULT ")]
    assert run.returncode == 0 and found, f"{run.stdout}\n{run.stderr}\n{stack.text()[-4000:]}"
    done = json.loads(found[-1][len("RESULT ") :])
    assert done["output"] == ANSWER
    # The tool ran once, in `jamjet worker`.
    assert (stack.project / "tool-calls.log").read_text() == "Lisbon\n"
    execution = httpx.get(f"{stack.url}/executions/{done['execution_id']}", timeout=5).json()
    assert execution["status"] == "completed"
    events = httpx.get(f"{stack.url}/executions/{done['execution_id']}/events", timeout=5).json()["events"]
    completed = Counter(e["kind"]["node_id"] for e in events if e["kind"]["type"] == "node_completed")
    assert completed == Counter({n: 1 for n in NODES})

    # Nothing but the ticks ran the engine's steps: the two model steps from the
    # queue, and the tool gate (a condition) in a walk, once the first had run.
    assert failed == []
    ran = [t for t in ticks if t.get("ran")]
    assert ran, ticks
    assert all(t["background"] == "off" for t in ran)
    assert sum(t["engine_steps"]["slow"] for t in ran) == 2, ran
    assert sum(t["engine_steps"]["quick"] for t in ran) == 1, ran
    say("request-driven", f"{len(ticks)} ticks ({len(ran)} ran) over {took:.1f} s")
