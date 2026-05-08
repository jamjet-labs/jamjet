"""End-to-end run() through entry.py for stateless DurableAgent."""
import pytest

from jamjet import resume, run
from jamjet.decorators import DurableAgent


# Module-level class so class_ref resolves via importlib
@DurableAgent(stateless=True)
class _Echo:
    async def run(self, q: str) -> str:
        return f"echo:{q}"


@pytest.mark.asyncio
async def test_run_resolves_class():
    out = await run(_Echo, "hi")
    assert out.output == "echo:hi"


@pytest.mark.asyncio
async def test_run_resolves_spec_directly():
    out = await run(_Echo.__jamjet_spec__, "hi")
    assert out.output == "echo:hi"


@pytest.mark.asyncio
async def test_resume_with_same_execution_id_short_circuits():
    out1 = await run(_Echo, "x", execution_id="ex-replay-test-1")
    out2 = await resume(_Echo.__jamjet_spec__, "ex-replay-test-1")
    assert out2.output == out1.output
