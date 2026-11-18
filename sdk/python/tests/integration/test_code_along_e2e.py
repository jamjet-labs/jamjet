"""Every step of the code-along series, on the real JamJet stack and the scripted model.

Opt-in. It starts `jamjet dev --modules venue_tools` in a temp project, on free
ports, with the engine binary named by JAMJET_SERVER_PATH:

    JAMJET_E2E=1 JAMJET_SERVER_PATH=<path to jamjet-server> \\
        uv run pytest tests/integration/test_code_along_e2e.py -q -s

No API key and no network: every model call is answered from a script in
code_along/scripts. The tools are code_along/venue_tools.py.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

import pytest
import yaml

pytestmark = pytest.mark.skipif(
    os.environ.get("JAMJET_E2E") != "1" or not os.environ.get("JAMJET_SERVER_PATH"),
    reason="opt-in: set JAMJET_E2E=1 and JAMJET_SERVER_PATH=<jamjet-server binary>",
)

SOURCE = Path(__file__).parent / "code_along"
FIND = "Find me a venue in Lisbon for 2026-12-11 and tell me its other free dates."
BOOK = "Book a venue in Lisbon for 2026-12-11 and pay a 500 deposit."
CANCEL = "Please cancel my booking at Casa do Rio."
LOOP = "Keep checking dates until you find something."
EXEC_ID = re.compile(r"exec_[0-9a-f]{32}")


def say(step: str, text: str) -> None:
    """Print what a learner would see; shown with `pytest -s`."""
    print(f"\n--- {step}\n{text.rstrip()}", flush=True)


def lines(path: Path) -> list[str]:
    return path.read_text().splitlines() if path.exists() else []


# ── The project: venue_tools.py and the scripts, in a temp folder ─────────────


@pytest.fixture(scope="module")
def project(tmp_path_factory: pytest.TempPathFactory) -> Path:
    root = tmp_path_factory.mktemp("code_along")
    shutil.copy(SOURCE / "venue_tools.py", root)
    shutil.copytree(SOURCE / "scripts", root / "scripts")
    (root / "home").mkdir()
    return root


@pytest.fixture(scope="module")
def child_env(project: Path) -> dict[str, str]:
    """The environment for every process: a fresh HOME and no model keys."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.endswith(("_API_KEY", "_API_BASE", "_BASE_URL"))
        and not (k.startswith("JAMJET_") and k != "JAMJET_SERVER_PATH")
        and k != "DATABASE_URL"
    }
    env.update(HOME=str(project / "home"), PYTHONUNBUFFERED="1", NO_COLOR="1", COLUMNS="200")
    return env


@pytest.fixture(scope="module")
def tools(project: Path, child_env: dict[str, str]) -> Any:
    """venue_tools imported from the project, as the worker imports it."""
    with pytest.MonkeyPatch.context() as mp:
        for key in set(os.environ) - set(child_env):
            mp.delenv(key)
        mp.setenv("HOME", child_env["HOME"])
        mp.syspath_prepend(str(project))
        sys.modules.pop("venue_tools", None)
        import venue_tools

        yield venue_tools
        sys.modules.pop("venue_tools", None)


@pytest.fixture
def fresh_logs(tools: Any) -> None:
    tools.PAYMENTS.unlink(missing_ok=True)
    tools.CANCELLATIONS.unlink(missing_ok=True)


def script(project: Path, name: str) -> str:
    return f"scripted/{project / 'scripts' / name}"


def answer(project: Path, name: str) -> str:
    """The script's last turn: the answer the agent should give."""
    turns = yaml.safe_load((project / "scripts" / name).read_text())["turns"]
    return turns[-1]["content"]


def venue_agent(project: Path, tools: Any, name: str, *tool_names: str, **governance: Any) -> Any:
    from jamjet import Agent

    return Agent(
        "venue-booker",
        model=script(project, name),
        tools=[getattr(tools, t) for t in tool_names],
        instructions="You book event venues. Use the tools.",
        strategy="react",
        timeout_seconds=120,
        **governance,
    )


# ── The stack: `jamjet dev`, which starts the sidecar, engine and worker ──────


def free_port() -> int:
    while True:
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        if port not in (7700, 4280):
            return port


def processes() -> list[tuple[int, int, str]]:
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,command="], capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        pid, ppid, command = line.split(None, 2)
        rows.append((int(pid), int(ppid), command))
    return rows


class DevStack:
    def __init__(self, project: Path, env: dict[str, str]) -> None:
        self.project = project
        self.env = env
        self.port = free_port()
        self.sidecar_port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.db = project / ".jamjet" / "runtime.db"
        self.proc: subprocess.Popen[bytes] | None = None
        self.log: Path | None = None
        self.children: dict[int, str] = {}  # every child ever started: pid -> command
        self.starts = 0

    def start(self) -> None:
        self.starts += 1
        self.log = self.project / f"jamjet-dev-{self.starts}.log"
        # No --db: the engine keeps its database in .jamjet/runtime.db under the
        # project folder, as it does for a learner, so a restart finds the same runs.
        argv = ["dev", "--modules", "venue_tools", "--port", str(self.port), "--sidecar-port", str(self.sidecar_port)]
        with self.log.open("w") as out:
            self.proc = subprocess.Popen(
                [sys.executable, "-m", "jamjet", *argv],
                cwd=self.project,
                env=self.env,
                stdout=out,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        deadline = time.monotonic() + 90
        while True:
            # Note each child as it appears, so stop() can reach it even if the start fails.
            self.children.update({pid: cmd for pid, ppid, cmd in processes() if ppid == self.proc.pid})
            if "worker ready." in self.log.read_text():
                return
            if self.proc.poll() is not None or time.monotonic() > deadline:
                pytest.fail(f"jamjet dev did not start:\n{self.log.read_text()[-3000:]}")
            time.sleep(0.25)

    def engine_pid(self) -> int:
        binary = os.path.realpath(os.environ["JAMJET_SERVER_PATH"])
        assert self.proc is not None
        pids = [pid for pid, ppid, cmd in processes() if ppid == self.proc.pid and cmd.startswith(binary)]
        assert len(pids) == 1, f"expected one engine under jamjet dev, found {pids}"
        return pids[0]

    def kill_engine(self) -> int:
        """kill -9 the engine. jamjet dev sees it exit and stops the rest."""
        assert self.proc is not None
        pid = self.engine_pid()
        os.kill(pid, signal.SIGKILL)
        self.proc.wait(timeout=60)
        return pid

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)  # jamjet dev stops its children
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=10)
        # Each child leads its own process group. Kill any that is still there.
        alive = {pid: cmd for pid, _, cmd in processes()}
        for pid, command in self.children.items():
            if alive.get(pid) == command:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except OSError:
                    pass

    def jamjet(self, *args: str) -> str:
        """Run a jamjet command against this stack; return what it printed."""
        done = subprocess.run(
            [sys.executable, "-m", "jamjet", *args, "--runtime", self.url],
            cwd=self.project,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert done.returncode == 0, f"jamjet {' '.join(args)} exited {done.returncode}:\n{done.stdout}{done.stderr}"
        return done.stdout

    def execution(self, exec_id: str) -> dict[str, Any]:
        from jamjet.client import JamjetClient

        async def get() -> dict[str, Any]:
            async with JamjetClient(self.url) as client:
                return await client.get_execution(exec_id)

        return asyncio.run(get())


@pytest.fixture(scope="module")
def stack(project: Path, child_env: dict[str, str], request: pytest.FixtureRequest) -> DevStack:
    dev = DevStack(project, child_env)
    request.addfinalizer(dev.stop)
    dev.start()
    assert dev.db.exists()
    say("jamjet dev", f"engine {dev.url}, sidecar port {dev.sidecar_port}, db {dev.db}")
    return dev


class DurableRun:
    """agent.run_durable in a thread, so the test can approve while it waits."""

    def __init__(self, agent: Any, prompt: str, url: str) -> None:
        self.result: Any = None
        self.error: BaseException | None = None
        self.thread = threading.Thread(target=self._run, args=(agent, prompt, url), daemon=True)
        self.thread.start()

    def _run(self, agent: Any, prompt: str, url: str) -> None:
        try:
            self.result = asyncio.run(agent.run_durable(prompt, max_turns=4, runtime_url=url))
        except BaseException as exc:  # noqa: BLE001 - reported by the test
            self.error = exc

    def join(self) -> None:
        self.thread.join(timeout=120)
        assert not self.thread.is_alive(), "run_durable did not return"


def wait_for_approval(stack: DevStack, run: DurableRun) -> tuple[str, str]:
    """Poll `jamjet approvals` until the run waits on pay_deposit."""
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if run.error is not None:
            raise run.error
        listing = stack.jamjet("approvals")
        found = EXEC_ID.search(listing)
        if found and "pay_deposit" in listing:
            return found.group(0), listing
        time.sleep(0.5)
    pytest.fail(f"no approval appeared; last listing:\n{listing}")


def book_with_approval(stack: DevStack, project: Path, tools: Any) -> dict[str, Any]:
    """Step 6: a durable booking that pauses at pay_deposit, then gets approved."""
    tools.PAYMENTS.unlink(missing_ok=True)
    agent = venue_agent(
        project,
        tools,
        "book.yaml",
        "search_venues",
        "check_dates",
        "pay_deposit",
        policy={"require_approval_for": ["pay_deposit"]},
    )
    run = DurableRun(agent, BOOK, stack.url)
    exec_id, listing = wait_for_approval(stack, run)
    paid_before_approval = lines(tools.PAYMENTS)
    approved = stack.jamjet("approve", exec_id, "-d", "approved", "--as", "tester")
    run.join()
    if run.error is not None:
        raise run.error
    return {
        "exec_id": exec_id,
        "listing": listing,
        "approved": approved,
        "result": run.result,
        "paid_before_approval": paid_before_approval,
        "payments": lines(tools.PAYMENTS),
    }


@pytest.fixture(scope="module")
def booked(stack: DevStack, project: Path, tools: Any) -> dict[str, Any]:
    return book_with_approval(stack, project, tools)


# ── The steps ─────────────────────────────────────────────────────────────────


def test_step1_agent_with_two_tools(project: Path, tools: Any, fresh_logs: None) -> None:
    agent = venue_agent(project, tools, "find.yaml", "search_venues", "check_dates")
    result = agent.run_sync(FIND)
    say("step 1", f"{[c['tool'] for c in result.tool_calls]}\n{result.output}")

    assert result.output == answer(project, "find.yaml")
    assert [c["tool"] for c in result.tool_calls] == ["search_venues", "check_dates"]
    assert "Casa do Rio" in result.tool_calls[0]["output"]
    assert json.loads(result.tool_calls[1]["output"]) == ["2026-12-04", "2026-12-11"]


def test_step2_pay_deposit_with_no_policy_pays(project: Path, tools: Any, fresh_logs: None) -> None:
    agent = venue_agent(project, tools, "book.yaml", "search_venues", "check_dates", "pay_deposit")
    result = agent.run_sync(BOOK)
    say("step 2", f"{result.output}\npayments.log: {lines(tools.PAYMENTS)}")

    assert result.output == answer(project, "book.yaml")
    assert [c["tool"] for c in result.tool_calls] == ["search_venues", "check_dates", "pay_deposit"]
    assert len(lines(tools.PAYMENTS)) == 1


def test_step3_in_process_block_hides_the_tool(project: Path, tools: Any, fresh_logs: None) -> None:
    from jamjet.model.scripted import ScriptedModelError

    agent = venue_agent(
        project,
        tools,
        "cancel.yaml",
        "search_venues",
        "check_dates",
        "cancel_booking",
        policy={"blocked_tools": ["cancel_*"]},
    )
    # Main's in-process path removes a blocked tool from the tools the model is
    # offered, so the model can never ask for it. The scripted model refuses to
    # call a tool it was not offered, and says which tools it got.
    with pytest.raises(ScriptedModelError) as info:
        agent.run_sync(CANCEL)
    say("step 3 (in-process)", str(info.value))

    assert "offered only: search_venues, check_dates" in str(info.value)
    assert not tools.CANCELLATIONS.exists()


def test_step3_durable_block_stops_the_run(stack: DevStack, project: Path, tools: Any, fresh_logs: None) -> None:
    agent = venue_agent(
        project,
        tools,
        "cancel.yaml",
        "search_venues",
        "check_dates",
        "cancel_booking",
        policy={"blocked_tools": ["cancel_*"]},
    )
    # Main's durable path: the model asks for cancel_booking, the engine blocks
    # it, and the whole run fails. The model gets no turn to explain.
    with pytest.raises(RuntimeError) as info:
        asyncio.run(agent.run_durable(CANCEL, max_turns=4, runtime_url=stack.url))
    exec_id = info.value.execution_id  # type: ignore[attr-defined]
    trail = stack.jamjet("audit", exec_id)
    say("step 3 (durable)", f"{info.value}\n{trail}")

    assert "ended in non-completed state: failed" in str(info.value)
    assert stack.execution(exec_id)["status"] == "failed"
    assert "model asked for cancel_booking" in trail
    assert "cancel_booking blocked" in trail
    assert "No tool ran." in trail
    assert not tools.CANCELLATIONS.exists()


def test_step4_in_process_approval_fails_closed(project: Path, tools: Any, fresh_logs: None) -> None:
    from jamjet.agents.governance import ApprovalNotEnforceableError

    agent = venue_agent(
        project,
        tools,
        "book.yaml",
        "search_venues",
        "check_dates",
        "pay_deposit",
        policy={"require_approval_for": ["pay_deposit"]},
    )
    with pytest.raises(ApprovalNotEnforceableError) as info:
        agent.run_sync(BOOK)
    say("step 4", str(info.value))

    assert "Use run_durable()" in str(info.value)
    assert lines(tools.PAYMENTS) == []


def test_step5_token_budget_stops_a_loop(project: Path, tools: Any) -> None:
    from jamjet.model import BudgetExceededError
    from jamjet.model.scripted import ScriptedModelError

    # Without a cap the loop runs until the script runs out.
    loose = venue_agent(project, tools, "loop.yaml", "check_dates", max_iterations=50)
    with pytest.raises(ScriptedModelError, match="the script has 20 turns"):
        loose.run_sync(LOOP)

    # 520 tokens a turn: after four turns the total is 2080, so the fifth call is refused.
    capped = venue_agent(project, tools, "loop.yaml", "check_dates", max_iterations=50, budget={"tokens": 2000})
    with pytest.raises(BudgetExceededError) as info:
        capped.run_sync(LOOP)
    say("step 5", str(info.value))

    assert str(info.value) == "budget exceeded: tokens 2080 >= limit 2000"


def test_step6_durable_run_pauses_for_approval(project: Path, tools: Any, booked: dict[str, Any]) -> None:
    say("step 6: jamjet approvals", booked["listing"])
    say("step 6: jamjet approve", booked["approved"])
    say("step 6: answer", f"{booked['result'].output}\npayments.log: {booked['payments']}")

    exec_id = booked["exec_id"]
    first = booked["listing"].splitlines()[0]
    assert exec_id in first
    assert "pay_deposit" in booked["listing"]
    assert booked["paid_before_approval"] == []
    assert booked["approved"].startswith("approved")
    assert f"execution={exec_id}" in booked["approved"]
    assert booked["result"].execution_id == exec_id
    assert booked["result"].output == answer(project, "book.yaml")
    assert [c["tool"] for c in booked["result"].tool_calls] == ["search_venues", "check_dates", "pay_deposit"]
    assert len(booked["payments"]) == 1


def test_step7_engine_crash_then_approve_pays_once(
    stack: DevStack, project: Path, tools: Any, capfd: pytest.CaptureFixture[str]
) -> None:
    tools.PAYMENTS.unlink(missing_ok=True)
    agent = venue_agent(
        project,
        tools,
        "book.yaml",
        "search_venues",
        "check_dates",
        "pay_deposit",
        policy={"require_approval_for": ["pay_deposit"]},
    )
    run = DurableRun(agent, BOOK, stack.url)
    exec_id, _ = wait_for_approval(stack, run)

    killed = stack.kill_engine()
    time.sleep(2)  # the client is now polling a dead engine and must not give up
    assert run.thread.is_alive(), f"run_durable ended while the engine was down: {run.error!r}"
    assert lines(tools.PAYMENTS) == []

    stack.start()  # same project folder, so the same .jamjet/runtime.db
    # The engine may not have recovered the run yet: wait until it lists it.
    deadline = time.monotonic() + 30
    while True:
        listing = stack.jamjet("approvals")
        if exec_id in listing:
            break
        assert time.monotonic() < deadline, f"{exec_id} never reappeared; last listing:\n{listing}"
        time.sleep(0.5)
    approved = stack.jamjet("approve", exec_id, "-d", "approved", "--as", "tester")
    run.join()
    if run.error is not None:
        raise run.error
    execution = stack.execution(exec_id)
    stderr = capfd.readouterr().err
    trail = stack.jamjet("audit", exec_id)
    say(
        "step 7",
        f"kill -9 engine pid {killed}\nclient stderr:\n{stderr}\nclient answer: {run.result.output}\n"
        f"after restart, jamjet approvals:\n{listing}\n{approved}\n{trail}\n"
        f"payments.log: {lines(tools.PAYMENTS)}",
    )

    shown = f"execution {exec_id})"
    assert f"jamjet: lost the engine at {stack.url}; waiting for it to come back ({shown}" in stderr
    assert stderr.count("jamjet: lost the engine at") == 1
    assert stderr.count("jamjet: the engine is back; resuming the wait") == 1
    assert run.result.execution_id == exec_id
    assert run.result.output == answer(project, "book.yaml")
    assert execution["status"] == "completed"
    assert execution["current_state"]["last_model_output"] == answer(project, "book.yaml")
    assert len(lines(tools.PAYMENTS)) == 1
    assert "pay_deposit ran 1 time" in trail


def test_step8_audit_shows_the_approver_and_one_payment(stack: DevStack, booked: dict[str, Any]) -> None:
    trail = stack.jamjet("audit", booked["exec_id"])
    say("step 8: jamjet audit", trail)

    assert "approved by tester" in trail
    assert "pay_deposit ran 1 time, approved by tester" in trail
    assert "search_venues ran 1 time" in trail
    assert "check_dates ran 1 time" in trail


def normalised(audit: dict[str, Any]) -> list[dict[str, str]]:
    """Audit rows with times dropped and ids and process ids masked."""
    masks = [
        (EXEC_ID, "exec_<id>"),
        (re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"), "<uuid>"),
        (re.compile(r"pid \d+"), "pid <n>"),
    ]
    rows = []
    for row in audit["rows"]:
        clean = {}
        for key, value in row.items():
            if key == "time":
                continue
            for pattern, mask in masks:
                value = pattern.sub(mask, value)
            clean[key] = value
        rows.append(clean)
    return rows


def test_step9_reruns_give_the_same_audit(stack: DevStack, project: Path, tools: Any, booked: dict[str, Any]) -> None:
    runs = [booked, book_with_approval(stack, project, tools), book_with_approval(stack, project, tools)]
    audits = [json.loads(stack.jamjet("audit", r["exec_id"], "--json")) for r in runs]

    differ = sorted(
        {
            key
            for other in audits[1:]
            for a, b in zip(audits[0]["rows"], other["rows"], strict=False)
            for key in a
            if a[key] != b.get(key)
        }
    )
    say(
        "step 9",
        f"runs: {[r['exec_id'] for r in runs]}\n"
        f"row fields that differ before masking: {differ}\n"
        f"payments.log lines per run: {[len(r['payments']) for r in runs]}",
    )

    assert [len(r["payments"]) for r in runs] == [1, 1, 1]
    assert len({r["exec_id"] for r in runs}) == 3
    for r in runs[1:]:
        assert r["paid_before_approval"] == []
        assert r["result"].output == answer(project, "book.yaml")
        assert "pay_deposit ran 1 time, approved by tester" in stack.jamjet("audit", r["exec_id"])
    first = normalised(audits[0])
    assert [row["step"] for row in first][-1] == "final answer"
    for audit in audits[1:]:
        assert normalised(audit) == first
        assert audit["tool_runs"] == audits[0]["tool_runs"]
