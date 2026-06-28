"""Tests for `jamjet dev` full-stack orchestration (DevStack).

These exercise the orchestration LOGIC in isolation — start order, env wiring,
the health-gate and graceful teardown — with a fake process spawner and a fake
health probe injected. No real processes are started.
"""

from __future__ import annotations

import pytest

from jamjet.cli.dev_stack import DevStack, DevStackError

# ---------------------------------------------------------------------------
# Test doubles
# ---------------------------------------------------------------------------


class FakeProcess:
    """A stand-in for a spawned process exposing the ManagedProcess surface."""

    def __init__(self, name: str, *, dies: bool = False) -> None:
        self.name = name
        self.stdout = None  # no log pump in tests
        self.terminated = False
        self.killed = False
        self.waited = False
        self.returncode: int | None = None
        self._dies = dies

    def poll(self) -> int | None:
        if self.terminated:
            self.returncode = 0
            return 0
        if self.killed:
            self.returncode = -9
            return -9
        if self._dies:
            self.returncode = 1
            return 1
        return None

    def terminate(self) -> None:
        self.terminated = True

    def kill(self) -> None:
        self.killed = True

    def wait(self, timeout: float | None = None) -> int:
        self.waited = True
        if self.returncode is None:
            self.returncode = 0
        return self.returncode


class FakeClock:
    """Deterministic monotonic clock advanced only by sleep()."""

    def __init__(self) -> None:
        self.t = 0.0

    def monotonic(self) -> float:
        return self.t

    def sleep(self, dt: float) -> None:
        self.t += dt


class Recorder:
    """A fake spawner + health-probe that records an ordered event log."""

    def __init__(
        self,
        *,
        healthy_after: dict[str, int] | None = None,
        never_healthy: list[str] | None = None,
        die: set[str] | None = None,
    ) -> None:
        self.events: list[tuple[str, str]] = []
        self.specs: dict[str, object] = {}
        self.procs: list[FakeProcess] = []
        self.healthy_after = healthy_after or {}
        self.never_healthy = list(never_healthy or [])
        self.die = die or set()
        self._probe_counts: dict[str, int] = {}

    def spawn(self, spec) -> FakeProcess:  # noqa: ANN001
        self.events.append(("spawn", spec.name))
        self.specs[spec.name] = spec
        proc = FakeProcess(spec.name, dies=spec.name in self.die)
        self.procs.append(proc)
        return proc

    def probe(self, url: str) -> bool:
        self.events.append(("probe", url))
        for frag in self.never_healthy:
            if frag in url:
                return False
        n = self._probe_counts.get(url, 0) + 1
        self._probe_counts[url] = n
        for frag, need in self.healthy_after.items():
            if frag in url:
                if n >= need:
                    self.events.append(("healthy", url))
                    return True
                return False
        self.events.append(("healthy", url))
        return True


def _make_stack(rec: Recorder, clock: FakeClock, **kw) -> DevStack:
    defaults = dict(
        binary="/fake/jamjet-server",
        base_env={"PATH": "/usr/bin"},
        spawn=rec.spawn,
        health_probe=rec.probe,
        sleep=clock.sleep,
        monotonic=clock.monotonic,
        log=lambda *_a, **_k: None,
        health_timeout=30.0,
        poll_interval=0.25,
        readiness_wait=0.0,
        shutdown_grace=5.0,
    )
    defaults.update(kw)
    return DevStack(**defaults)


def _spawn_order(rec: Recorder) -> list[str]:
    return [name for (kind, name) in rec.events if kind == "spawn"]


def _first_index(rec: Recorder, pred) -> int:
    return next(i for i, e in enumerate(rec.events) if pred(e))


# ---------------------------------------------------------------------------
# Start order + env wiring
# ---------------------------------------------------------------------------


def test_start_order_sidecar_health_gated_before_engine_then_worker():
    # Sidecar reports healthy only after a few polls, so we can assert the
    # gate completes BEFORE the engine is spawned.
    rec = Recorder(healthy_after={":4280/health": 3})
    clock = FakeClock()
    _make_stack(rec, clock, sidecar_port=4280).start()

    assert _spawn_order(rec) == ["sidecar", "engine", "worker"]

    sidecar_healthy = _first_index(rec, lambda e: e[0] == "healthy" and ":4280" in e[1])
    engine_spawn = _first_index(rec, lambda e: e == ("spawn", "engine"))
    worker_spawn = _first_index(rec, lambda e: e == ("spawn", "worker"))
    assert sidecar_healthy < engine_spawn < worker_spawn


def test_engine_env_carries_model_seam_url():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, sidecar_port=4280).start()
    engine_env = rec.specs["engine"].env
    assert engine_env["JAMJET_MODEL_SEAM_URL"] == "http://127.0.0.1:4280"


def test_engine_env_has_port_and_dev_mode():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, port=7799).start()
    env = rec.specs["engine"].env
    assert env["PORT"] == "7799"
    assert env["JAMJET_PORT"] == "7799"
    assert env["JAMJET_DEV_MODE"] == "1"


def test_engine_is_health_gated():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, port=7700).start()
    assert any(k == "probe" and ":7700/health" in u for (k, u) in rec.events)


def test_worker_argv_includes_modules_passthrough():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, modules="weather_agent").start()
    argv = rec.specs["worker"].argv
    assert "worker" in argv
    assert "--modules" in argv
    assert argv[argv.index("--modules") + 1] == "weather_agent"


def test_worker_argv_omits_modules_when_none():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, modules=None).start()
    argv = rec.specs["worker"].argv
    assert "--modules" not in argv


# ---------------------------------------------------------------------------
# Failure modes — fail loud + tear down
# ---------------------------------------------------------------------------


def test_sidecar_never_healthy_fails_loud_and_tears_down():
    rec = Recorder(never_healthy=[":4280/health"])
    clock = FakeClock()
    stack = _make_stack(rec, clock, sidecar_port=4280)

    with pytest.raises(DevStackError) as exc_info:
        stack.start()

    assert "sidecar" in str(exc_info.value).lower()
    # Engine + worker must NOT have been started after the sidecar failed.
    assert _spawn_order(rec) == ["sidecar"]
    # The half-started sidecar must have been torn down (no orphan).
    assert rec.procs[0].terminated or rec.procs[0].killed


def test_engine_dies_before_healthy_fails_loud_and_tears_down_sidecar():
    rec = Recorder(die={"engine"})
    clock = FakeClock()
    stack = _make_stack(rec, clock)

    with pytest.raises(DevStackError) as exc_info:
        stack.start()

    assert "engine" in str(exc_info.value).lower()
    # The healthy sidecar that was already up must be torn down.
    sidecar_proc = next(p for p in rec.procs if p.name == "sidecar")
    assert sidecar_proc.terminated or sidecar_proc.killed
    # Worker never started.
    assert "worker" not in _spawn_order(rec)


# ---------------------------------------------------------------------------
# Flags: --no-sidecar / --no-worker / --engine-only
# ---------------------------------------------------------------------------


def test_no_sidecar_skips_sidecar_and_does_not_set_seam_env():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, enable_sidecar=False).start()

    assert "sidecar" not in _spawn_order(rec)
    assert "JAMJET_MODEL_SEAM_URL" not in rec.specs["engine"].env


def test_no_sidecar_pops_inherited_seam_env():
    """A stale exported JAMJET_MODEL_SEAM_URL must not leak through to the engine
    when we are not managing a sidecar — else the engine fails closed."""
    rec = Recorder()
    clock = FakeClock()
    _make_stack(
        rec,
        clock,
        enable_sidecar=False,
        base_env={"JAMJET_MODEL_SEAM_URL": "http://stale:1234"},
    ).start()
    assert "JAMJET_MODEL_SEAM_URL" not in rec.specs["engine"].env


def test_no_worker_skips_worker():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, enable_worker=False).start()
    spawned = _spawn_order(rec)
    assert "worker" not in spawned
    assert "sidecar" in spawned
    assert "engine" in spawned


def test_engine_only_starts_only_the_engine():
    rec = Recorder()
    clock = FakeClock()
    _make_stack(rec, clock, enable_sidecar=False, enable_worker=False).start()
    assert _spawn_order(rec) == ["engine"]
    assert "JAMJET_MODEL_SEAM_URL" not in rec.specs["engine"].env


# ---------------------------------------------------------------------------
# Teardown — no orphans
# ---------------------------------------------------------------------------


def test_shutdown_terminates_all_started_processes():
    rec = Recorder()
    clock = FakeClock()
    stack = _make_stack(rec, clock)
    stack.start()

    stack.shutdown()

    assert len(rec.procs) == 3
    for proc in rec.procs:
        assert proc.terminated, f"{proc.name} was not terminated"
        assert proc.waited, f"{proc.name} was not waited on"


def test_shutdown_is_idempotent():
    rec = Recorder()
    clock = FakeClock()
    stack = _make_stack(rec, clock)
    stack.start()
    stack.shutdown()
    # Second call must not raise and must not double-terminate.
    stack.shutdown()
    assert all(p.terminated for p in rec.procs)


def test_keyboard_interrupt_during_start_tears_down():
    """If startup is interrupted (Ctrl+C), already-started processes are torn down."""

    class Boom:
        calls = 0

        def __call__(self, url: str) -> bool:
            Boom.calls += 1
            raise KeyboardInterrupt

    rec = Recorder()
    clock = FakeClock()
    stack = _make_stack(rec, clock, health_probe=Boom())

    with pytest.raises(KeyboardInterrupt):
        stack.start()

    # The sidecar (first spawned) must be torn down.
    assert rec.procs[0].terminated or rec.procs[0].killed


# ---------------------------------------------------------------------------
# CLI command wiring
# ---------------------------------------------------------------------------


def test_dev_command_engine_only_wires_flags(monkeypatch):
    import jamjet.cli.dev_stack as dev_stack
    import jamjet.cli.main as cli

    captured: dict = {}

    class FakeStack:
        def __init__(self, **kw):
            captured.update(kw)

        def run(self) -> None:
            return None

    monkeypatch.setattr(cli, "_find_server_binary", lambda: "/fake/jamjet-server")
    monkeypatch.setattr(dev_stack, "DevStack", FakeStack)

    from typer.testing import CliRunner

    result = CliRunner().invoke(cli.app, ["dev", "--engine-only"])
    assert result.exit_code == 0, result.output
    assert captured["enable_sidecar"] is False
    assert captured["enable_worker"] is False


def test_dev_command_passes_sidecar_port_and_modules(monkeypatch):
    import jamjet.cli.dev_stack as dev_stack
    import jamjet.cli.main as cli

    captured: dict = {}

    class FakeStack:
        def __init__(self, **kw):
            captured.update(kw)

        def run(self) -> None:
            return None

    monkeypatch.setattr(cli, "_find_server_binary", lambda: "/fake/jamjet-server")
    monkeypatch.setattr(dev_stack, "DevStack", FakeStack)
    # Pretend uvicorn is importable so the sidecar path is taken without a warning exit.
    monkeypatch.setattr("importlib.util.find_spec", lambda name: object())

    from typer.testing import CliRunner

    result = CliRunner().invoke(
        cli.app,
        ["dev", "--sidecar-port", "4999", "--modules", "myapp.tools"],
    )
    assert result.exit_code == 0, result.output
    assert captured["sidecar_port"] == 4999
    assert captured["modules"] == "myapp.tools"
    assert captured["enable_sidecar"] is True
    assert captured["enable_worker"] is True
