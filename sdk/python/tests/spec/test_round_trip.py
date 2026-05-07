"""Property-based round-trip tests across all spec types."""
from hypothesis import given
from hypothesis import strategies as st

from jamjet.spec import (
    AgentSpec,
    AgentStrategy,
    DurabilityConfig,
    DurableAgentSpec,
    LLMConfig,
    MemoryConfig,
    MethodSpec,
)

providers = st.sampled_from(["openai", "anthropic", "google", "ollama", "openai_compatible"])
strategy_names = st.sampled_from([
    "plan-and-execute", "react", "critic", "reflection", "consensus", "debate", "custom",
])


@st.composite
def llm_configs(draw):
    return LLMConfig(provider=draw(providers), model=draw(st.text(min_size=1, max_size=20)))


@given(llm_configs())
def test_llm_round_trip(cfg):
    assert LLMConfig.model_validate_json(cfg.model_dump_json()) == cfg


@given(st.builds(MemoryConfig))
def test_memory_round_trip(cfg):
    assert MemoryConfig.model_validate_json(cfg.model_dump_json()) == cfg


@given(st.builds(DurabilityConfig))
def test_durability_round_trip(cfg):
    assert DurabilityConfig.model_validate_json(cfg.model_dump_json()) == cfg


@given(llm_configs(), st.text(min_size=1, max_size=20), strategy_names)
def test_agent_round_trip(llm, name, strat_name):
    a = AgentSpec(name=name, llm=llm, strategy=AgentStrategy(name=strat_name))
    assert AgentSpec.model_validate_json(a.model_dump_json()) == a


@given(llm_configs(), st.text(min_size=1, max_size=20))
def test_durable_agent_round_trip(llm, name):
    a = DurableAgentSpec(
        name=name, llm=llm, class_ref="m:C",
        methods=[MethodSpec(name="run", is_entrypoint=True)],
    )
    assert DurableAgentSpec.model_validate_json(a.model_dump_json()) == a
