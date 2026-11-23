"""Step 10 of the venue-booking example, on the real JamJet stack.

A venue's reply tells the agent to pay 2400 when we asked for 500. With
an approval gate on pay_deposit the payment waits for a person, and a
person who says no leaves payments.log empty.

Opt-in, like the other example tests on the real stack:

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_venue_step10_e2e.py -q -s
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from ..test_venue_booking_example import ROOT, TEST_SIGNING_KEY
from .devstack import (
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

STEP = "step-10-untrusted-reply"


@pytest.fixture(scope="module")
def project(tmp_path_factory: pytest.TempPathFactory) -> Path:
    root = tmp_path_factory.mktemp("venue_step10")
    for name in ("agent.py", "venue_tools.py", "script.yaml"):
        shutil.copy(ROOT / STEP / name, root)
    shutil.copytree(ROOT / STEP / "replies", root / "replies")
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

    @property
    def error(self) -> BaseException | None:
        """wait_for_approval reads this to notice an agent that died."""
        if self.alive():
            return None
        out, err = self.proc.communicate()
        return AssertionError(f"agent.py exited early ({self.proc.returncode}):\n{out}\n{err[-3000:]}")

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=10)


def test_step10_gate_holds_the_payment_the_reply_asked_for(
    stack: DevStack, project: Path, child_env: dict[str, str], request: pytest.FixtureRequest
) -> None:
    payments = project / "payments.log"
    agent = AgentProcess(project, child_env, stack.url)
    request.addfinalizer(agent.stop)

    exec_id, listing = wait_for_approval(stack, agent)  # type: ignore[arg-type]
    say("step 10: jamjet approvals", listing)
    assert agent.alive(), "agent.py should still be waiting"
    # The request shows what the reply asked for, not the 500 we asked for.
    assert '"amount": 2400' in listing
    assert lines(payments) == [], "nothing may be paid while the payment waits"

    args = printed_approve_args(listing, exec_id)
    args[args.index("--decision") + 1] = "rejected"
    say("step 10: jamjet approve --decision rejected", stack.jamjet(*args))
    out, err = agent.proc.communicate(timeout=120)
    say("step 10: agent.py stdout", out)

    assert lines(payments) == [], f"a rejected payment was paid:\n{out}\n{err[-2000:]}"
    assert out.strip().splitlines()[-1] == "payments.log: 0 line(s)"
    audit = stack.jamjet("audit", exec_id)
    say("step 10: jamjet audit", audit)
    assert "rejected by tester" in audit
