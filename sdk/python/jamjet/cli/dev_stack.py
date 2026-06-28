"""Local-stack orchestration for `jamjet dev`.

`jamjet dev` brings up the whole durable dev loop with one command:

  1. the model **sidecar** (`uvicorn jamjet.model.sidecar_server:app`) FIRST,
     health-gated on `GET /health` — the engine fails closed at startup if
     ``JAMJET_MODEL_SEAM_URL`` is set but the sidecar is unreachable, so the
     sidecar must be up and healthy BEFORE the engine starts;
  2. the **engine** (`jamjet-server`) with ``JAMJET_MODEL_SEAM_URL`` wired to the
     sidecar so durable model calls route through the governed seam;
  3. a **worker** (`jamjet worker`) draining the ``python_tool`` queue.

On Ctrl+C (or SIGTERM) every child is torn down as a group — SIGTERM, then
SIGKILL after a grace period — leaving no orphans.

The orchestration LOGIC (start order, env wiring, the health-gate, teardown) is
factored into :class:`DevStack`, which takes an injectable process *spawner* and
a *health probe* so it can be unit-tested without starting real processes.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from typing import Protocol


class DevStackError(RuntimeError):
    """A managed process failed to start (names the failing process)."""


@dataclass
class ProcessSpec:
    """How to launch one managed process, plus how to know it is ready."""

    name: str
    argv: list[str]
    env: dict[str, str]
    health_url: str | None = None


class ManagedProcess(Protocol):
    """The subset of a spawned process the orchestrator relies on."""

    name: str

    def poll(self) -> int | None: ...

    def terminate(self) -> None: ...

    def kill(self) -> None: ...

    def wait(self, timeout: float | None = None) -> int: ...


Spawner = Callable[[ProcessSpec], ManagedProcess]
HealthProbe = Callable[[str], bool]
Logger = Callable[..., None]


# ── Real process + spawner ────────────────────────────────────────────────────


class _GroupProcess:
    """Wrap ``subprocess.Popen`` so terminate/kill hit the whole process group.

    Each child is started in its own session (``start_new_session=True``) so a
    single ``killpg`` reaps the child *and* anything it forked (uvicorn workers,
    cargo, etc.). Falls back to per-process signalling where process groups are
    unavailable (e.g. Windows).
    """

    def __init__(self, popen: subprocess.Popen, name: str) -> None:
        self._p = popen
        self.name = name
        self.stdout = popen.stdout  # piped; read by the log-prefixing thread

    @property
    def pid(self) -> int:
        return self._p.pid

    def poll(self) -> int | None:
        return self._p.poll()

    def _signal_group(self, sig: int) -> None:
        killpg = getattr(os, "killpg", None)
        getpgid = getattr(os, "getpgid", None)
        if killpg is not None and getpgid is not None:
            try:
                killpg(getpgid(self._p.pid), sig)
                return
            except (ProcessLookupError, PermissionError, OSError):
                pass
        try:
            self._p.send_signal(sig)
        except (ProcessLookupError, OSError):
            pass

    def terminate(self) -> None:
        self._signal_group(signal.SIGTERM)

    def kill(self) -> None:
        self._signal_group(getattr(signal, "SIGKILL", signal.SIGTERM))

    def wait(self, timeout: float | None = None) -> int:
        return self._p.wait(timeout=timeout)


def real_spawn(spec: ProcessSpec) -> ManagedProcess:
    """Spawn a process in its own session, piping stdout/stderr for log-prefixing."""
    popen = subprocess.Popen(  # noqa: S603 - argv is built from trusted internal values
        spec.argv,
        env=spec.env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        bufsize=1,
        text=True,
    )
    return _GroupProcess(popen, spec.name)


def http_health_probe(url: str) -> bool:
    """Return True iff ``GET url`` answers 2xx AND its body affirms health.

    The ``/health`` contract promises a healthy JSON body, so a 2xx alone is not
    enough -- a 200 with a wrong, absent, or non-JSON body is NOT healthy (fail
    closed). Both probed services are accepted in their real documented shapes:
    the model sidecar returns ``{"ok": true}`` and the engine returns
    ``{"status": "ok", ...}``. Total: never raises -- any error (unreachable,
    non-JSON, non-dict body) returns False.
    """
    try:
        with urllib.request.urlopen(url, timeout=2) as resp:  # noqa: S310 - fixed localhost URL
            if not (200 <= resp.status < 300):
                return False
            body = resp.read()
    except (urllib.error.URLError, OSError, ValueError):
        return False
    try:
        payload = json.loads(body)
    except (ValueError, TypeError):
        return False
    if not isinstance(payload, dict):
        return False
    return payload.get("ok") is True or payload.get("status") == "ok"


# ── Log prefixing ─────────────────────────────────────────────────────────────

_ANSI = {
    "sidecar": "\033[36m",  # cyan
    "engine": "\033[32m",  # green
    "worker": "\033[35m",  # magenta
}
_ANSI_RESET = "\033[0m"


def _prefix(name: str) -> str:
    if sys.stdout.isatty() and name in _ANSI:
        return f"{_ANSI[name]}[{name}]{_ANSI_RESET} "
    return f"[{name}] "


# ── The orchestrator ──────────────────────────────────────────────────────────


@dataclass
class DevStack:
    """Start order + env wiring + health-gate + graceful teardown for `jamjet dev`.

    The spawner and health probe are injectable so the sequencing can be tested
    without real processes (see ``tests/test_cli_dev.py``).
    """

    binary: str
    base_env: Mapping[str, str]
    port: int = 7700
    sidecar_port: int = 4280
    db_url: str | None = None
    modules: str | None = None
    runtime_url: str | None = None
    enable_sidecar: bool = True
    enable_worker: bool = True
    # Injectables (defaulted to the real implementations).
    spawn: Spawner = real_spawn
    health_probe: HealthProbe = http_health_probe
    sleep: Callable[[float], None] = time.sleep
    monotonic: Callable[[], float] = time.monotonic
    log: Logger = print
    # Tunables.
    health_timeout: float = 30.0
    poll_interval: float = 0.25
    readiness_wait: float = 0.5
    shutdown_grace: float = 5.0
    rust_log: str = "info"
    # Internal state.
    _started: list[tuple[ManagedProcess, ProcessSpec]] = field(default_factory=list, init=False)
    _log_threads: list[threading.Thread] = field(default_factory=list, init=False)
    _shut_down: bool = field(default=False, init=False)

    # -- spec building (pure; observable through the injected spawner) ----------

    def _seam_url(self) -> str:
        return f"http://127.0.0.1:{self.sidecar_port}"

    def _runtime_url(self) -> str:
        return self.runtime_url or f"http://127.0.0.1:{self.port}"

    def build_specs(self) -> list[ProcessSpec]:
        """Build the ordered process specs with all env wiring resolved."""
        specs: list[ProcessSpec] = []

        if self.enable_sidecar:
            specs.append(
                ProcessSpec(
                    name="sidecar",
                    argv=[
                        sys.executable,
                        "-m",
                        "uvicorn",
                        "jamjet.model.sidecar_server:app",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        str(self.sidecar_port),
                    ],
                    env=dict(self.base_env),
                    health_url=f"{self._seam_url()}/health",
                )
            )

        engine_env = dict(self.base_env)
        engine_env["PORT"] = str(self.port)
        engine_env["JAMJET_PORT"] = str(self.port)
        engine_env["JAMJET_DEV_MODE"] = "1"
        engine_env.setdefault("RUST_LOG", self.rust_log)
        if self.db_url:
            engine_env["DATABASE_URL"] = self.db_url
        if self.enable_sidecar:
            # Route durable model calls through the governed seam.
            engine_env["JAMJET_MODEL_SEAM_URL"] = self._seam_url()
        else:
            # Never let a stale exported seam URL make the engine fail closed
            # when we are NOT managing a sidecar.
            engine_env.pop("JAMJET_MODEL_SEAM_URL", None)
        specs.append(
            ProcessSpec(
                name="engine",
                argv=[self.binary],
                env=engine_env,
                health_url=f"http://127.0.0.1:{self.port}/health",
            )
        )

        if self.enable_worker:
            worker_argv = [
                sys.executable,
                "-m",
                "jamjet",
                "worker",
                "--runtime",
                self._runtime_url(),
                "--queue",
                "python_tool",
            ]
            if self.modules:
                worker_argv += ["--modules", self.modules]
            specs.append(
                ProcessSpec(
                    name="worker",
                    argv=worker_argv,
                    env=dict(self.base_env),
                    health_url=None,
                )
            )

        return specs

    # -- lifecycle --------------------------------------------------------------

    def start(self) -> None:
        """Start every process in order, health-gating each before the next.

        Fails LOUD (``DevStackError`` naming the process) and tears down anything
        already started if a process never becomes ready.
        """
        try:
            for spec in self.build_specs():
                self.log(f"Starting {spec.name}...")
                try:
                    proc = self.spawn(spec)
                except DevStackError:
                    raise
                except Exception as e:  # noqa: BLE001 - normalize ANY spawn failure
                    # The spawner failed to even start the process (e.g. the server
                    # binary is missing -> FileNotFoundError, or an OSError from the
                    # OS). Re-raise as DevStackError so the CLI -- which only catches
                    # DevStackError -- prints the intended dev-stack failure message
                    # instead of a raw traceback. Already-started children are torn
                    # down by the outer handler below.
                    raise DevStackError(f"{spec.name} failed to spawn: {e}") from e
                self._started.append((proc, spec))
                self._attach_logs(proc, spec.name)
                if spec.health_url is not None:
                    self._health_gate(proc, spec)
                else:
                    self._readiness_check(proc, spec)
                self.log(f"  {spec.name} ready.")
        except BaseException:
            # Includes DevStackError and KeyboardInterrupt during startup.
            self.shutdown()
            raise

    def _health_gate(self, proc: ManagedProcess, spec: ProcessSpec) -> None:
        assert spec.health_url is not None
        deadline = self.monotonic() + self.health_timeout
        while True:
            rc = proc.poll()
            if rc is not None:
                raise DevStackError(f"{spec.name} exited before it was ready (exit code {rc}).")
            if self.health_probe(spec.health_url):
                return
            if self.monotonic() >= deadline:
                raise DevStackError(
                    f"{spec.name} failed to start: {spec.health_url} did not become healthy "
                    f"within {self.health_timeout:.0f}s."
                )
            self.sleep(self.poll_interval)

    def _readiness_check(self, proc: ManagedProcess, spec: ProcessSpec) -> None:
        """For processes with no health endpoint (the worker): a short settle then
        confirm it did not die immediately."""
        self.sleep(self.readiness_wait)
        rc = proc.poll()
        if rc is not None:
            raise DevStackError(f"{spec.name} exited immediately (exit code {rc}).")

    def shutdown(self) -> None:
        """Terminate every started process as a group, SIGKILL stragglers.

        Guarantees termination with no infinite block and no orphan: every live
        child gets a SIGTERM, a bounded shared grace wait (the wait timeout is
        ALWAYS floored above zero, so a SIGTERM-ignoring child can never make
        ``wait()`` block forever), then a final UNCONDITIONAL SIGKILL sweep with
        a short bounded wait over anything still alive.
        """
        if self._shut_down:
            return
        self._shut_down = True

        # Phase 1: SIGTERM all live children (reverse start order).
        for proc, spec in reversed(self._started):
            if proc.poll() is None:
                self.log(f"Stopping {spec.name}...")
                try:
                    proc.terminate()
                except Exception:  # noqa: BLE001 - best-effort teardown
                    pass

        # Phase 2: bounded grace wait for clean SIGTERM exits (shared deadline).
        # The timeout is ALWAYS floored above zero: once an earlier child has
        # exhausted the shared deadline, later children still get a tiny bounded
        # wait (never timeout=None), so wait() can never block forever on a
        # SIGTERM-ignoring child.
        deadline = self.monotonic() + self.shutdown_grace
        for proc, spec in reversed(self._started):
            remaining = deadline - self.monotonic()
            try:
                proc.wait(timeout=max(remaining, 0.1))
            except subprocess.TimeoutExpired:
                pass
            except Exception:  # noqa: BLE001 - never let teardown raise
                pass

        # Phase 3: UNCONDITIONAL SIGKILL sweep over anything still alive, then a
        # short bounded wait so every survivor is reaped (no orphan, no hang).
        # This is what fires for a child that ignored SIGTERM during Phase 2.
        for proc, spec in reversed(self._started):
            if proc.poll() is None:
                try:
                    proc.kill()
                except Exception:  # noqa: BLE001 - best-effort teardown
                    pass
                try:
                    proc.wait(timeout=max(self.shutdown_grace, 1.0))
                except Exception:  # noqa: BLE001 - never let teardown raise (incl. TimeoutExpired)
                    pass

    def run(self) -> None:
        """Start the stack, then block until a child exits or Ctrl+C, then tear down."""
        self.start()
        try:
            self._wait_for_exit()
        except KeyboardInterrupt:
            self.log("\nShutting down JamJet dev stack...")
        finally:
            self.shutdown()

    def _wait_for_exit(self) -> None:
        while True:
            for proc, spec in self._started:
                rc = proc.poll()
                if rc is not None:
                    self.log(f"{spec.name} exited (code {rc}); shutting down the stack.")
                    return
            self.sleep(0.5)

    # -- log prefixing ----------------------------------------------------------

    def _attach_logs(self, proc: ManagedProcess, name: str) -> None:
        stdout = getattr(proc, "stdout", None)
        if stdout is None:
            return
        prefix = _prefix(name)

        def _pump() -> None:
            try:
                for line in iter(stdout.readline, ""):
                    if line == "":
                        break
                    self.log(prefix + line.rstrip("\n"))
            except Exception:  # noqa: BLE001 - reader thread must never crash the stack
                pass

        thread = threading.Thread(target=_pump, name=f"logs-{name}", daemon=True)
        thread.start()
        self._log_threads.append(thread)
