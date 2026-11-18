"""Steps 05 and 06 of the venue-booking example, on the real JamJet stack.

Opt-in. It copies the step folder to a temp dir, starts
`jamjet dev --modules venue_tools` there on free ports, runs agent.py
as a learner would, and approves the payment from "another terminal":

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_venue_booking_example_e2e.py -q -s

The agent finds the engine through JAMJET_RUNTIME_URL, so the test can
point it at a free port. Learners leave it unset and get port 7700.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import pytest

from ..test_venue_booking_example import ROOT, TEST_SIGNING_KEY, expected, normalise
from .devstack import (
    EXEC_ID,
    DevStack,
    lines,
    printed_approve_args,
    say,
    wait_for_approval,
)

pytestmark = pytest.mark.skipif(
    os.environ.get("JAMJET_E2E") != "1" or not os.environ.get("JAMJET_SERVER_PATH"),
    reason="opt-in: set JAMJET_E2E=1 and JAMJET_SERVER_PATH=<jamjet-server binary>",
)

STEP = "step-05-ask-first"


@pytest.fixture(scope="module")
def project(tmp_path_factory: pytest.TempPathFactory) -> Path:
    root = tmp_path_factory.mktemp("venue_step05")
    for name in ("agent.py", "venue_tools.py", "script.yaml"):
        shutil.copy(ROOT / STEP / name, root)
    (root / "home").mkdir()
    return root


@pytest.fixture(scope="module")
def child_env(project: Path) -> dict[str, str]:
    """A fresh HOME, no model keys, no JAMJET_* settings but the ones we set."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not (k.startswith("JAMJET_") and k != "JAMJET_SERVER_PATH")
        and k not in ("DATABASE_URL", "VENUE_MODEL")
    }
    env.update(HOME=str(project / "home"), PYTHONUNBUFFERED="1", NO_COLOR="1", COLUMNS="200")
    env["JAMJET_SERVER_PATH"] = os.path.realpath(os.environ["JAMJET_SERVER_PATH"])
    env["JAMJET_AUDIT_SIGNING_KEY"] = TEST_SIGNING_KEY
    return env


@pytest.fixture(scope="module")
def stack(project: Path, child_env: dict[str, str], request: pytest.FixtureRequest) -> DevStack:
    dev = DevStack(project, child_env)
    request.addfinalizer(dev.stop)
    dev.start()
    say("jamjet dev", f"engine {dev.url}, sidecar port {dev.sidecar_port}")
    return dev


class AgentProcess:
    """python agent.py in the project folder, pointed at the test engine."""

    def __init__(self, project: Path, env: dict[str, str], url: str) -> None:
        self.proc = subprocess.Popen(
            [sys.executable, "agent.py"],
            cwd=project,
            env={**env, "JAMJET_RUNTIME_URL": url},
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def alive(self) -> bool:
        return self.proc.poll() is None

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=10)


class _Waiting:
    """Lets wait_for_approval see an agent that died instead of waiting."""

    def __init__(self, agent: AgentProcess) -> None:
        self.agent = agent

    @property
    def error(self) -> BaseException | None:
        if self.agent.alive():
            return None
        out, err = self.agent.proc.communicate()
        return AssertionError(f"agent.py exited early ({self.agent.proc.returncode}):\n{out}\n{err[-3000:]}")


def test_step05_durable_pauses_then_pays_once(
    stack: DevStack, project: Path, child_env: dict[str, str], request: pytest.FixtureRequest
) -> None:
    payments = project / "payments.log"
    agent = AgentProcess(project, child_env, stack.url)
    request.addfinalizer(agent.stop)

    exec_id, listing = wait_for_approval(stack, _Waiting(agent))  # type: ignore[arg-type]
    assert agent.alive(), "agent.py should still be waiting"
    assert lines(payments) == [], "nothing may be paid before the approval"

    approved = stack.jamjet(*printed_approve_args(listing, exec_id))
    out, err = agent.proc.communicate(timeout=120)
    say("step 05: jamjet approvals", listing)
    say("step 05: jamjet approve", approved)
    say("step 05: agent.py stdout", out)

    assert agent.proc.returncode == 0, f"agent.py exited {agent.proc.returncode}\n{out}\n{err[-3000:]}"
    assert EXEC_ID.search(out).group(0) == exec_id
    assert normalise(out) == expected(STEP)
    assert len(lines(payments)) == 1
    assert "pay_deposit" in listing


STEP06 = "step-06-block"


@pytest.fixture(scope="module")
def project06(tmp_path_factory: pytest.TempPathFactory) -> Path:
    root = tmp_path_factory.mktemp("venue_step06")
    for name in ("agent.py", "venue_tools.py", "script.yaml"):
        shutil.copy(ROOT / STEP06 / name, root)
    (root / "home").mkdir()
    return root


@pytest.fixture(scope="module")
def stack06(project06: Path, request: pytest.FixtureRequest) -> DevStack:
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not (k.startswith("JAMJET_") and k != "JAMJET_SERVER_PATH")
        and k not in ("DATABASE_URL", "VENUE_MODEL")
    }
    env.update(HOME=str(project06 / "home"), PYTHONUNBUFFERED="1", NO_COLOR="1", COLUMNS="200")
    env["JAMJET_SERVER_PATH"] = os.path.realpath(os.environ["JAMJET_SERVER_PATH"])
    env["JAMJET_AUDIT_SIGNING_KEY"] = TEST_SIGNING_KEY
    dev = DevStack(project06, env)
    request.addfinalizer(dev.stop)
    dev.start()
    return dev


def test_step06_engine_blocks_the_cancellation(stack06: DevStack, project06: Path) -> None:
    agent = AgentProcess(project06, stack06.env, stack06.url)
    try:
        out, err = agent.proc.communicate(timeout=120)
    finally:
        agent.stop()
    say("step 06: agent.py stdout", out)

    assert agent.proc.returncode == 0, f"agent.py exited {agent.proc.returncode}\n{out}\n{err[-3000:]}"
    exec_id = EXEC_ID.search(out).group(0)
    trail = stack06.jamjet("audit", exec_id)
    say("step 06: jamjet audit", trail)

    assert "run failed:" in out
    assert out.rstrip().endswith("cancellations.log: 0 line(s)")
    assert not (project06 / "cancellations.log").exists()
    assert "cancel_booking blocked" in trail
    assert stack06.execution(exec_id)["status"] == "failed"
    assert normalise(out) == expected(STEP06)


STEP07 = "step-07-engine-crash"
STEP08 = "step-08-what-it-remembers"
STEP09 = "step-09-audit"
LOST = "jamjet: lost the engine at"
BACK = "jamjet: the engine is back; resuming the wait"


@pytest.fixture(scope="module")
def project07(tmp_path_factory: pytest.TempPathFactory) -> Path:
    root = tmp_path_factory.mktemp("venue_step07")
    for name in ("agent.py", "venue_tools.py", "script.yaml"):
        shutil.copy(ROOT / STEP07 / name, root)
    (root / "home").mkdir()
    return root


@pytest.fixture(scope="module")
def stack07(project07: Path, request: pytest.FixtureRequest) -> DevStack:
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not (k.startswith("JAMJET_") and k != "JAMJET_SERVER_PATH")
        and k not in ("DATABASE_URL", "VENUE_MODEL")
    }
    env.update(HOME=str(project07 / "home"), PYTHONUNBUFFERED="1", NO_COLOR="1", COLUMNS="200")
    env["JAMJET_SERVER_PATH"] = os.path.realpath(os.environ["JAMJET_SERVER_PATH"])
    env["JAMJET_AUDIT_SIGNING_KEY"] = TEST_SIGNING_KEY
    dev = DevStack(project07, env)
    request.addfinalizer(dev.stop)
    dev.start()
    return dev


@pytest.fixture(scope="module")
def crashed(stack07: DevStack, project07: Path, request: pytest.FixtureRequest) -> dict[str, str]:
    """Step 07: kill the engine while the approval waits, restart, approve."""
    agent = AgentProcess(project07, stack07.env, stack07.url)
    request.addfinalizer(agent.stop)
    exec_id, _ = wait_for_approval(stack07, _Waiting(agent))  # type: ignore[arg-type]
    assert lines(project07 / "payments.log") == []

    killed = stack07.kill_engine()
    time.sleep(2)  # agent.py is now polling a dead engine and must keep waiting
    assert agent.alive(), "agent.py should keep waiting while the engine is down"

    stack07.start()  # same folder, so the same .jamjet/runtime.db
    deadline = time.monotonic() + 30
    while True:
        listing = stack07.jamjet("approvals")
        if exec_id in listing:
            break
        assert time.monotonic() < deadline, f"{exec_id} never reappeared:\n{listing}"
        time.sleep(0.5)
    approved = stack07.jamjet(*printed_approve_args(listing, exec_id))
    out, err = agent.proc.communicate(timeout=120)
    say("step 07: kill -9 engine pid", str(killed))
    say("step 07: agent.py stderr", err)
    say("step 07: agent.py stdout", out)
    return {"exec_id": exec_id, "out": out, "err": err, "rc": str(agent.proc.returncode), "approved": approved}


def test_step07_engine_crash_then_approve_pays_once(
    crashed: dict[str, str], project07: Path, stack07: DevStack
) -> None:
    out, err = crashed["out"], crashed["err"]
    assert crashed["rc"] == "0", f"agent.py exited {crashed['rc']}\n{out}\n{err[-3000:]}"
    assert err.count(LOST) == 1
    assert err.count(BACK) == 1
    assert f"{LOST} {stack07.url}; waiting for it to come back (execution {crashed['exec_id']})" in err
    assert normalise(out) == expected(STEP07)
    assert len(lines(project07 / "payments.log")) == 1


def test_step08_executions_and_events_show_the_log(crashed: dict[str, str], stack07: DevStack) -> None:
    exec_id = crashed["exec_id"]
    listing = stack07.jamjet("executions")
    events = stack07.jamjet("events", exec_id)
    say("step 08: jamjet executions", listing)
    say("step 08: jamjet events", events)

    row = next(ln for ln in listing.splitlines() if exec_id in ln)
    assert "completed" in row
    assert "tool_approval_required" in events
    assert "approval_received" in events
    assert "workflow_completed" in events


def test_step09_audit_says_who_approved(crashed: dict[str, str], stack07: DevStack) -> None:
    exec_id = crashed["exec_id"]
    trail = stack07.jamjet("audit", exec_id)
    as_json = stack07.jamjet("audit", exec_id, "--json")
    say("step 09: jamjet audit", trail)
    say("step 09: jamjet audit --json", as_json)

    assert "pay_deposit ran 1 time, approved by tester at" in trail
    assert "tester" in as_json
    last = [ln for ln in trail.splitlines() if ln.strip()][-1]
    saved = [ln for ln in expected(STEP09).splitlines() if ln.strip()][-1]
    assert normalise(last) == normalise(saved)
    assert {"execution", "rows", "tool_runs"} <= set(json.loads(as_json))


def test_steps_07_to_09_share_the_step_05_code() -> None:
    for step in (STEP07, STEP08, STEP09):
        for name in ("agent.py", "venue_tools.py", "script.yaml"):
            assert (ROOT / step / name).read_bytes() == (ROOT / STEP / name).read_bytes(), (step, name)
