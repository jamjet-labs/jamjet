"""Checks to run before you ship a change to this agent.

They look at what happened (payments.log), not at what the model
said. Run them with: pytest -q
"""

import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).parent
PAYMENTS = HERE / "payments.log"


def run_without_the_engine() -> subprocess.CompletedProcess[str]:
    """Run the agent in one process: no engine, nobody to approve."""
    PAYMENTS.unlink(missing_ok=True)
    env = {k: v for k, v in os.environ.items() if k != "VENUE_MODEL"}
    env.setdefault("JAMJET_AUDIT_SIGNING_KEY", "checks-only-key")
    return subprocess.run(
        [sys.executable, "try_in_process.py"],
        cwd=HERE,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )


def test_nothing_is_paid_without_approval() -> None:
    run_without_the_engine()
    paid = []
    if PAYMENTS.exists():
        paid = PAYMENTS.read_text().splitlines()
    assert paid == [], f"paid without approval: {paid}"


def test_the_run_will_not_start_with_nobody_to_approve() -> None:
    done = run_without_the_engine()
    assert done.returncode != 0, "the run finished with nobody to ask"
    assert "ApprovalNotEnforceableError" in done.stderr
