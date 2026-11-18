"""Step 05 of the venue-booking example, on the real JamJet stack.

Opt-in. It copies the step folder to a temp dir, starts
`jamjet dev --modules venue_tools` there on free ports, runs agent.py
as a learner would, and approves the payment from "another terminal":

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_venue_booking_example_e2e.py -q -s

The agent finds the engine through JAMJET_RUNTIME_URL, so the test can
point it at a free port. Learners leave it unset and get port 7700.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
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
