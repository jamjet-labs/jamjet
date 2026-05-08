import pytest

from jamjet.runtime.stub import CloudRuntime, JavaRuntime, RustRuntime
from jamjet.spec import AgentSpec, LLMConfig


@pytest.mark.parametrize("cls", [CloudRuntime, JavaRuntime, RustRuntime])
async def test_stub_raises_not_implemented(cls):
    rt = cls()
    spec = AgentSpec(name="x", llm=LLMConfig(provider="openai", model="gpt-4o"))
    with pytest.raises(NotImplementedError, match="Phase 5"):
        await rt.execute(spec, input="x")


@pytest.mark.parametrize("cls", [CloudRuntime, JavaRuntime, RustRuntime])
async def test_stub_resume_raises(cls):
    rt = cls()
    spec = AgentSpec(name="x", llm=LLMConfig(provider="openai", model="gpt-4o"))
    with pytest.raises(NotImplementedError, match="Phase 5"):
        await rt.resume(spec, "ex1")
