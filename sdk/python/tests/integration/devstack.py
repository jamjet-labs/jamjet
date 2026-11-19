"""Helpers that drive a real `jamjet dev` stack from a test.

Shared by the code-along e2e test and the venue-booking example tests. They
start the stack on free ports, run a durable agent in a thread, wait for an
approval and approve it the way a learner does.
"""

from __future__ import annotations

import asyncio
import os
import re
import shlex
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

import pytest

EXEC_ID = re.compile(r"exec_[0-9a-f]{32}")


def say(step: str, text: str) -> None:
    """Print what a learner would see; shown with `pytest -s`."""
    print(f"\n--- {step}\n{text.rstrip()}", flush=True)


def lines(path: Path) -> list[str]:
    return path.read_text().splitlines() if path.exists() else []


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


def printed_approve_args(listing: str, exec_id: str) -> list[str]:
    """The approve command `jamjet approvals` printed (digest and all) with
    `--as tester` added, as the arguments after `jamjet`."""
    lines_ = [ln.strip() for ln in listing.splitlines() if ln.strip().startswith("jamjet approve ")]
    mine = [ln for ln in lines_ if exec_id in ln]
    assert len(mine) == 1, f"expected one approve command for {exec_id}; listing:\n{listing}"
    args = shlex.split(mine[0])
    assert "--action-digest" in mine[0], mine[0]
    return [*args[1:], "--as", "tester"]
