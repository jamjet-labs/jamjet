import pytest

from jamjet.runtime.stub import JavaRuntime, RustRuntime
from jamjet.spec import AgentSpec, LLMConfig

# NB: CloudRuntime is intentionally excluded — it no longer carries the generic
# "lands in Phase 5" stub message. It raises an honest "use Agent.deploy(...)"
# error instead (Track 7a-4); that behaviour is covered in test_deploy_redirect.py.


@pytest.mark.parametrize("cls", [JavaRuntime, RustRuntime])
async def test_stub_raises_not_implemented(cls):
    rt = cls()
    spec = AgentSpec(name="x", llm=LLMConfig(provider="openai", model="gpt-4o"))
    with pytest.raises(NotImplementedError, match="Phase 5"):
        await rt.execute(spec, input="x")


@pytest.mark.parametrize("cls", [JavaRuntime, RustRuntime])
async def test_stub_resume_raises(cls):
    rt = cls()
    spec = AgentSpec(name="x", llm=LLMConfig(provider="openai", model="gpt-4o"))
    with pytest.raises(NotImplementedError, match="Phase 5"):
        await rt.resume(spec, "ex1")
