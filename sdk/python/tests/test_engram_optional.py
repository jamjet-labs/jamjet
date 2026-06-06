import subprocess
import sys
import textwrap


def _run(code: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, "-c", textwrap.dedent(code)], capture_output=True, text=True)


def test_import_jamjet_without_engram():
    r = _run("""
        import sys
        sys.modules["engram"] = None  # simulate engram not installed
        import jamjet
        from jamjet.runtime.local import LocalRuntime
        from jamjet import DurableAgent, run
        print("OK")
    """)
    assert r.returncode == 0, f"import jamjet failed without engram:\n{r.stderr}"
    assert "OK" in r.stdout


def test_agent_memory_attr_without_engram_raises_clear_error():
    r = _run("""
        import sys
        sys.modules["engram"] = None
        import jamjet
        try:
            jamjet.AgentMemory       # lazy attribute access
            print("NO_ERROR")
        except ImportError as e:
            assert "memory" in str(e).lower(), str(e)
            print("OK")
    """)
    assert r.returncode == 0, r.stderr
    assert "OK" in r.stdout


def test_jamjet_memory_nomemory_imports_without_engram():
    r = _run("""
        import sys
        sys.modules["engram"] = None
        from jamjet.memory import NoMemory
        print("OK")
    """)
    assert r.returncode == 0, r.stderr
    assert "OK" in r.stdout


def test_run_no_memory_durable_agent_without_engram(tmp_path):
    """A no-memory @DurableAgent must run successfully even when engram is absent.

    Before the executor fix, `_run_durable_agent` imported `engram` unconditionally,
    so ANY durable agent execution raised ImportError when engram was not installed.
    This test exercises the full LocalRuntime._run_durable_agent path with
    sys.modules["engram"] = None to confirm the fix holds.
    """
    db = str(tmp_path / "ckpt.db")
    code = textwrap.dedent(f"""
        import sys, asyncio
        sys.modules["engram"] = None  # simulate engram not installed

        from jamjet.decorators import DurableAgent, task
        from jamjet.spec import DurabilityConfig, MemoryConfig
        from jamjet.runtime.local import LocalRuntime

        @DurableAgent(
            memory=MemoryConfig(backend="none"),
            durability=DurabilityConfig(checkpoint_every_step=True),
        )
        class _Trivial:
            @task(entry=True)
            async def run(self, x: int) -> int:
                return x + 1

        async def main():
            spec = _Trivial.__jamjet_spec__.model_copy(
                update={{"durability": DurabilityConfig(db_path={db!r}, checkpoint_every_step=True)}}
            )
            from jamjet.runtime.local import LocalRuntime
            rt = LocalRuntime()
            result = await rt.execute(spec, 41, execution_id="no-engram-test")
            assert result.output == 42, f"unexpected output {{result.output!r}}"
            print("RAN_OK")

        asyncio.run(main())
    """)
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    assert "RAN_OK" in r.stdout
