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

import yaml

TEST_SIGNING_KEY = "venue-example-test-key"
ROOT = Path(__file__).resolve().parents[3] / "examples" / "venue-booking-agent"


def run_step(
    folder: str,
    script: str = "agent.py",
    env: dict[str, str] | None = None,
    clear_logs: bool = True,
    allow_fail: bool = False,
    args: tuple[str, ...] = (),
) -> str:
    """Run one script of a step folder and return its stdout.

    With allow_fail, a non-zero exit is expected: the result is stdout plus
    stderr, and the caller checks the exit through the text it gets. args
    follow the script on the command line, so script="-c" runs code.
    """
    cwd = ROOT / folder
    for log in ("payments.log", "cancellations.log") if clear_logs else ():
        (cwd / log).unlink(missing_ok=True)
    child_env = {k: v for k, v in os.environ.items() if k != "VENUE_MODEL"}
    child_env["JAMJET_AUDIT_SIGNING_KEY"] = TEST_SIGNING_KEY
    child_env.update(env or {})
    done = subprocess.run(
        [sys.executable, script, *args],
        cwd=cwd,
        env=child_env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    if allow_fail:
        assert done.returncode != 0, f"{folder}/{script} should have failed\n{done.stdout}"
        return done.stdout + done.stderr
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


def test_step02_tools_run_in_order_and_match_expected_output():
    out = run_step("step-02-tools")
    assert out.splitlines()[0] == "model: scripted (no API key)"
    assert normalise(out) == expected("step-02-tools")
    called = [line.split("(")[0] for line in out.splitlines() if line.startswith("called ")]
    assert called == ["called search_venues", "called check_dates"]


def test_step03_pay_deposit_pays_once_with_no_checks():
    out = run_step("step-03-pay-deposit")
    assert out.splitlines()[-1] == "payments.log: 1 line(s)"
    assert normalise(out) == expected("step-03-pay-deposit")
    lines = (ROOT / "step-03-pay-deposit" / "payments.log").read_text().splitlines()
    assert len(lines) == 1
    assert lines[0].startswith("paid 500")
    assert "Casa do Rio" in lines[0]


def test_step03_second_run_starts_with_a_fresh_log():
    run_step("step-03-pay-deposit")
    out = run_step("step-03-pay-deposit", clear_logs=False)
    assert out.splitlines()[-1] == "payments.log: 1 line(s)"


def test_step04_budget_stops_the_loop():
    out = run_step("step-04-budget")
    assert out.splitlines()[0] == "model: scripted (no API key)"
    assert normalise(out) == expected("step-04-budget")
    script = yaml.safe_load((ROOT / "step-04-budget" / "script.yaml").read_text())
    used = int(re.search(r"tokens (\d+) >= limit", out).group(1))
    turns_run = used // 520
    assert "budget exceeded" in out
    assert 0 < turns_run < len(script["turns"])


def test_step05_in_process_refuses():
    out = run_step("step-05-ask-first", "try_in_process.py", allow_fail=True)
    last = out.strip().splitlines()[-1]
    assert normalise(last) == expected("step-05-ask-first", "expected-in-process.txt").strip()
    assert "ApprovalNotEnforceableError" in last
    assert not (ROOT / "step-05-ask-first" / "payments.log").exists()


def long_lines(paths: list[Path], limit: int = 72) -> list[str]:
    """Name every line over the limit, as file:line."""
    found = []
    for path in paths:
        for n, line in enumerate(path.read_text().splitlines(), 1):
            if len(line) > limit:
                found.append(f"{path.name}:{n}")
    return found


def test_long_line_check_catches_a_long_line(tmp_path):
    sample = tmp_path / "sample.py"
    sample.write_text("short\n" + "x" * 73 + "\n")
    assert long_lines([sample]) == ["sample.py:2"]


def test_example_code_stays_within_72_columns():
    paths = []
    for folders in ("*", "solutions/*"):
        paths += sorted(ROOT.glob(f"{folders}/*.py"))
        paths += sorted(ROOT.glob(f"{folders}/script.yaml"))
    assert any("solutions" in path.parts for path in paths)
    assert long_lines(paths) == []


def test_steps_07_to_09_share_the_step_05_code():
    for step in ("step-07-engine-crash", "step-08-what-it-remembers", "step-09-audit"):
        for name in ("agent.py", "venue_tools.py", "script.yaml"):
            same = (ROOT / step / name).read_bytes() == (ROOT / "step-05-ask-first" / name).read_bytes()
            assert same, (step, name)


# ── Challenge solutions ───────────────────────────────────────────────────────

EP1 = "solutions/ep1-weather-tool"
EP2 = "solutions/ep2-approval-above-300"

# Calls pay_small_deposit directly, with the amount from the command line,
# then prints the tool's answer and how many payments it wrote.
PAY_SMALL = """
import asyncio, sys
from venue_tools import PAYMENTS, pay_small_deposit
amount = float(sys.argv[1])
print(asyncio.run(pay_small_deposit(venue="Casa do Rio", amount=amount)))
print(len(PAYMENTS.read_text().splitlines()) if PAYMENTS.exists() else 0)
"""


def test_solution_ep1_checks_the_weather_before_it_pays():
    out = run_step(EP1)
    assert normalise(out) == expected(EP1)
    called = [line.split("(")[0] for line in out.splitlines() if line.startswith("called ")]
    assert called.index("called get_weather") < called.index("called pay_deposit")
    assert "Rain is forecast" in out


def test_solution_ep2_small_deposit_pays_up_to_300():
    out = run_step(EP2, "-c", args=(PAY_SMALL, "300"))
    assert out.splitlines() == ["Deposit of 300.0 paid for Casa do Rio.", "1"]


def test_solution_ep2_small_deposit_refuses_over_300_in_code():
    out = run_step(EP2, "-c", args=(PAY_SMALL, "500"))
    assert out.splitlines() == [
        "Refused: 500.0 is over 300. Use pay_deposit, which waits for approval.",
        "0",
    ]
    assert not (ROOT / EP2 / "payments.log").exists()


def test_solution_ep2_keeps_the_approval_gate_on_pay_deposit():
    code = (ROOT / EP2 / "agent.py").read_text()
    assert 'policy={"require_approval_for": ["pay_deposit"]}' in code
    assert "pay_small_deposit, pay_deposit]" in code
    script = yaml.safe_load((ROOT / EP2 / "script.yaml").read_text())
    paid = [c for turn in script["turns"] for c in turn.get("tool_calls", []) if c["name"].startswith("pay_")]
    assert paid == [{"name": "pay_small_deposit", "arguments": {"venue": "Casa do Rio", "amount": 200}}]
