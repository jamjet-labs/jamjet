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
