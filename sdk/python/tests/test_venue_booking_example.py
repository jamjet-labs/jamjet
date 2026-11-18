"""The venue-booking code-along example: each step folder runs offline.

Every step runs on the scripted model, with no API key. A step's stdout is
compared with the expected-output.txt saved in its folder.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

TEST_SIGNING_KEY = "venue-example-test-key"
ROOT = Path(__file__).resolve().parents[3] / "examples" / "venue-booking-agent"


def run_step(folder: str, script: str = "agent.py", env: dict[str, str] | None = None) -> str:
    """Run one script of a step folder and return its stdout."""
    cwd = ROOT / folder
    for log in ("payments.log", "cancellations.log"):
        (cwd / log).unlink(missing_ok=True)
    child_env = {k: v for k, v in os.environ.items() if k != "VENUE_MODEL"}
    child_env["JAMJET_AUDIT_SIGNING_KEY"] = TEST_SIGNING_KEY
    child_env.update(env or {})
    done = subprocess.run(
        [sys.executable, script],
        cwd=cwd,
        env=child_env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert done.returncode == 0, f"{folder}/{script} exited {done.returncode}\n{done.stdout}\n{done.stderr[-3000:]}"
    return done.stdout


def normalise(text: str) -> str:
    """Hide the parts of the output that change on every run."""
    text = re.sub(r"exec_[0-9a-f]{32}", "exec_<id>", text)
    text = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<uuid>", text)
    text = re.sub(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?", "<timestamp>", text)
    return re.sub(r"\b\d{2}:\d{2}:\d{2}\b", "<time>", text)


def expected(folder: str, name: str = "expected-output.txt") -> str:
    return (ROOT / folder / name).read_text()


def test_step01_first_agent_answers_from_the_script():
    out = run_step("step-01-first-agent")
    assert out.splitlines()[0] == "model: scripted (no API key)"
    assert normalise(out) == expected("step-01-first-agent")


def test_normalise_masks_ids_and_times_but_not_dates_or_amounts():
    uuid = "123e4567-e89b-12d3-a456-426614174000"
    text = (
        f"run exec_{'a' * 32} id {uuid} at 2026-11-18T10:20:30Z and "
        "2026-11-18T10:20:30+01:00 or 10:20:30; book 2026-12-11 pay 500"
    )
    assert normalise(text) == (
        "run exec_<id> id <uuid> at <timestamp> and <timestamp> or <time>; book 2026-12-11 pay 500"
    )
