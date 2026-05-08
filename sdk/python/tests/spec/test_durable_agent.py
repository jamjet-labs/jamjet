import pytest
from pydantic import ValidationError

from jamjet.spec import (
    DurableAgentSpec,
    LLMConfig,
    MemoryConfig,
    MethodSpec,
)


def _llm():
    return LLMConfig(provider="openai", model="gpt-4o")


def test_method_spec_defaults():
    m = MethodSpec(name="plan")
    assert m.is_step is True
    assert m.is_entrypoint is False


def test_minimal_durable_agent():
    a = DurableAgentSpec(
        name="TripPlanner",
        llm=_llm(),
        class_ref="myapp.agents:TripPlanner",
        methods=[MethodSpec(name="plan", is_entrypoint=True)],
    )
    assert a.kind == "durable_agent"
    assert a.durability.checkpoint_every_step is True


def test_class_ref_required():
    with pytest.raises(ValidationError):
        DurableAgentSpec(name="x", llm=_llm(), methods=[])


def test_round_trip_json():
    a = DurableAgentSpec(
        name="x",
        llm=_llm(),
        class_ref="m:C",
        methods=[MethodSpec(name="run", is_entrypoint=True)],
        memory=MemoryConfig(),
    )
    assert DurableAgentSpec.model_validate_json(a.model_dump_json()) == a


def test_kind_discriminator_serialized():
    a = DurableAgentSpec(
        name="x", llm=_llm(), class_ref="m:C",
        methods=[MethodSpec(name="run", is_entrypoint=True)],
    )
    data = a.model_dump()
    assert data["kind"] == "durable_agent"
