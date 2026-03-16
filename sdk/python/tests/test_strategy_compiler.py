"""
Strategy compiler tests (J2.11).

Tests cover:
- All six strategies compile to valid IR DAGs
- Limits block validation (missing → clear error)
- Agent-first YAML parsing round-trips through ir_compiler
- Node/edge structure invariants (start_node reachable, __finalize__ present, etc.)
- Unknown strategy name raises ValueError
- Strategy config keys influence compiled output
"""

from __future__ import annotations

import pytest

from jamjet.compiler.strategies import StrategyLimits, compile_strategy
from jamjet.workflow.ir_compiler import compile_yaml

# ── Helpers ───────────────────────────────────────────────────────────────────

DEFAULT_LIMITS = StrategyLimits(max_iterations=3, max_cost_usd=0.50, timeout_seconds=60)


def _assert_ir_invariants(ir: dict, strategy_name: str) -> None:
    """Check structural invariants that all compiled strategies must satisfy."""
    nodes = ir["nodes"]
    edges = ir["edges"]
    start = ir["start_node"]

    # start_node must exist in nodes
    assert start in nodes, f"start_node '{start}' not in nodes"

    # __finalize__ must exist
    assert "__finalize__" in nodes, "missing __finalize__ node"

    # __limit_exceeded__ must exist (strategy limits are always injected)
    assert "__limit_exceeded__" in nodes, "missing __limit_exceeded__ node"

    # edges must reference valid nodes (target 'end' is the graph terminal)
    node_ids = set(nodes.keys()) | {"end"}
    for e in edges:
        assert e["from"] in node_ids or e["from"] == "end", f"edge from unknown node: {e['from']}"
        assert e["to"] in node_ids, f"edge to unknown node: {e['to']}"

    # strategy_metadata must carry strategy_name
    assert ir.get("strategy_metadata", {}).get("strategy_name") == strategy_name

    # every node must have id and kind
    for node_id, node_def in nodes.items():
        assert "kind" in node_def, f"node '{node_id}' missing 'kind'"


# ── StrategyLimits ────────────────────────────────────────────────────────────


def test_limits_validation_ok():
    lim = StrategyLimits(max_iterations=5, max_cost_usd=1.0, timeout_seconds=120)
    lim.validate()  # must not raise


def test_limits_validation_bad_iterations():
    with pytest.raises(ValueError, match="max_iterations"):
        StrategyLimits(max_iterations=0, max_cost_usd=1.0, timeout_seconds=60).validate()


def test_limits_validation_bad_cost():
    with pytest.raises(ValueError, match="max_cost_usd"):
        StrategyLimits(max_iterations=3, max_cost_usd=-1.0, timeout_seconds=60).validate()


def test_limits_validation_bad_timeout():
    with pytest.raises(ValueError, match="timeout_seconds"):
        StrategyLimits(max_iterations=3, max_cost_usd=1.0, timeout_seconds=0).validate()


# ── Unknown strategy ──────────────────────────────────────────────────────────


def test_unknown_strategy_raises():
    with pytest.raises(ValueError, match="Unknown strategy"):
        compile_strategy("turbo-dream", {}, [], "gpt-4o", DEFAULT_LIMITS, "Do something", "agent-1")


def test_unknown_strategy_error_lists_known():
    try:
        compile_strategy("bad", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "agent-1")
        assert False, "Expected ValueError"  # noqa: B011
    except ValueError as exc:
        msg = str(exc)
        for name in ("plan-and-execute", "react", "critic", "reflection", "consensus", "debate"):
            assert name in msg, f"expected '{name}' in error message, got: {msg}"


# ── plan-and-execute ──────────────────────────────────────────────────────────


def test_plan_and_execute_compiles():
    result = compile_strategy(
        "plan-and-execute",
        {},
        ["search_web", "read_file"],
        "gpt-4o",
        DEFAULT_LIMITS,
        "Research and write a report",
        "agent-1",
    )
    nodes = result["nodes"]

    # Core structural nodes
    assert "__plan__" in nodes
    assert "__step_0__" in nodes
    assert "__step_1__" in nodes
    assert "__step_2__" in nodes  # max_iterations=3
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    # Should NOT have a step_3 (max_iterations=3)
    assert "__step_3__" not in nodes

    _assert_ir_invariants(result, "plan-and-execute")


def test_plan_and_execute_start_node():
    result = compile_strategy("plan-and-execute", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__plan__"


def test_plan_and_execute_cost_guards():
    result = compile_strategy("plan-and-execute", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    nodes = result["nodes"]
    # Cost guards: one per step
    for i in range(DEFAULT_LIMITS.max_iterations):
        assert f"__cost_guard_{i}__" in nodes, f"missing __cost_guard_{i}__"


def test_plan_and_execute_with_verifier():
    config = {"verifier_model": "claude-sonnet-4-6"}
    result = compile_strategy("plan-and-execute", config, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    nodes = result["nodes"]
    # Verifier nodes should be present for each step
    for i in range(DEFAULT_LIMITS.max_iterations):
        assert f"__verify_{i}__" in nodes, f"missing __verify_{i}__"


def test_plan_and_execute_limits_in_metadata():
    result = compile_strategy("plan-and-execute", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    meta = result["strategy_metadata"]
    assert meta["limits"]["max_iterations"] == 3
    assert meta["limits"]["max_cost_usd"] == 0.50
    assert meta["limits"]["timeout_seconds"] == 60


def test_plan_and_execute_respects_max_iterations():
    limits_5 = StrategyLimits(max_iterations=5, max_cost_usd=1.0, timeout_seconds=120)
    result = compile_strategy("plan-and-execute", {}, [], "gpt-4o", limits_5, "goal", "a")
    nodes = result["nodes"]
    assert "__step_4__" in nodes
    assert "__step_5__" not in nodes


# ── react ─────────────────────────────────────────────────────────────────────


def test_react_compiles():
    result = compile_strategy(
        "react", {}, ["calculator", "search"], "gpt-4o", DEFAULT_LIMITS, "Solve the problem step by step", "agent-react"
    )
    nodes = result["nodes"]

    assert "__think_0__" in nodes
    assert "__think_1__" in nodes
    assert "__think_2__" in nodes
    assert "__observe_0__" in nodes
    assert "__observe_1__" in nodes
    # No observe for the last iteration (goes straight to finalize)
    assert "__observe_2__" not in nodes
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    _assert_ir_invariants(result, "react")


def test_react_start_node():
    result = compile_strategy("react", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__think_0__"


def test_react_cost_guards():
    result = compile_strategy("react", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    nodes = result["nodes"]
    for i in range(DEFAULT_LIMITS.max_iterations):
        assert f"__react_guard_{i}__" in nodes


# ── critic ────────────────────────────────────────────────────────────────────


def test_critic_compiles():
    result = compile_strategy(
        "critic",
        {"critic_model": "claude-sonnet-4-6", "max_rounds": 2, "pass_threshold": 0.85},
        [],
        "gpt-4o",
        DEFAULT_LIMITS,
        "Write a report",
        "agent-critic",
    )
    nodes = result["nodes"]

    assert "__draft__" in nodes
    assert "__critic_0__" in nodes
    assert "__critic_1__" in nodes
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    _assert_ir_invariants(result, "critic")


def test_critic_start_node():
    result = compile_strategy("critic", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__draft__"


def test_critic_max_rounds_capped_by_max_iterations():
    # max_rounds=10 but max_iterations=3 → only 3 critic rounds
    limits = StrategyLimits(max_iterations=3, max_cost_usd=1.0, timeout_seconds=120)
    config = {"max_rounds": 10}
    result = compile_strategy("critic", config, [], "gpt-4o", limits, "goal", "a")
    nodes = result["nodes"]
    assert "__critic_2__" in nodes
    assert "__critic_3__" not in nodes


def test_critic_default_model_is_same_as_main():
    result = compile_strategy("critic", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    # When no critic_model in config, critic nodes should use the main model
    critic_node = result["nodes"]["__critic_0__"]
    assert critic_node["kind"]["model_ref"] == "gpt-4o"


def test_critic_custom_critic_model():
    config = {"critic_model": "claude-opus-4-6"}
    result = compile_strategy("critic", config, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    critic_node = result["nodes"]["__critic_0__"]
    assert critic_node["kind"]["model_ref"] == "claude-opus-4-6"


# ── Agent-first YAML → ir_compiler ───────────────────────────────────────────

PLAN_AND_EXECUTE_YAML = """
agent:
  id: research-agent
  strategy: plan-and-execute
  goal: Research AI safety and produce a summary
  tools:
    - search_web
    - read_document
  model: gpt-4o
  limits:
    max_iterations: 4
    max_cost_usd: 0.75
    timeout_seconds: 180
"""


def test_agent_yaml_compiles_to_ir():
    ir = compile_yaml(PLAN_AND_EXECUTE_YAML)
    assert ir["workflow_id"] == "research-agent"
    assert ir["start_node"] == "__plan__"
    assert "__plan__" in ir["nodes"]
    assert "__step_0__" in ir["nodes"]
    assert "__step_3__" in ir["nodes"]  # max_iterations=4 → steps 0..3
    assert "__step_4__" not in ir["nodes"]
    assert ir["strategy_metadata"]["strategy_name"] == "plan-and-execute"


def test_agent_yaml_missing_limits_raises():
    yaml_no_limits = """
agent:
  id: bad-agent
  strategy: react
  goal: Do something
  model: gpt-4o
"""
    with pytest.raises(ValueError, match="limits"):
        compile_yaml(yaml_no_limits)


def test_agent_yaml_partial_limits_raises():
    yaml_partial = """
agent:
  id: bad-agent
  strategy: react
  goal: Do something
  model: gpt-4o
  limits:
    max_iterations: 5
"""
    with pytest.raises(ValueError, match="max_cost_usd"):
        compile_yaml(yaml_partial)


def test_agent_yaml_missing_strategy_raises():
    yaml_no_strategy = """
agent:
  id: bad-agent
  goal: Do something
  model: gpt-4o
  limits:
    max_iterations: 5
    max_cost_usd: 0.5
    timeout_seconds: 60
"""
    with pytest.raises(ValueError, match="strategy"):
        compile_yaml(yaml_no_strategy)


def test_agent_yaml_unknown_strategy_raises():
    yaml_bad_strategy = """
agent:
  id: bad-agent
  strategy: turbo-dream
  goal: Do something
  model: gpt-4o
  limits:
    max_iterations: 5
    max_cost_usd: 0.5
    timeout_seconds: 60
"""
    with pytest.raises(ValueError, match="Unknown strategy"):
        compile_yaml(yaml_bad_strategy)


def test_agent_yaml_timeout_in_ir():
    ir = compile_yaml(PLAN_AND_EXECUTE_YAML)
    assert ir["timeouts"]["workflow_timeout"] == 180


def test_agent_yaml_strategy_labels():
    ir = compile_yaml(PLAN_AND_EXECUTE_YAML)
    assert ir["labels"]["jamjet.strategy"] == "plan-and-execute"
    assert ir["labels"]["jamjet.agent.id"] == "research-agent"


def test_regular_workflow_yaml_still_works():
    regular_yaml = """
workflow:
  id: my-workflow
  version: 1.0.0
nodes:
  start:
    type: model
    model: gpt-4o
    prompt: Hello world
    next: end
"""
    ir = compile_yaml(regular_yaml)
    assert ir["workflow_id"] == "my-workflow"
    assert "start" in ir["nodes"]
    # Should NOT have strategy_metadata
    assert "strategy_metadata" not in ir


# ── reflection ─────────────────────────────────────────────────────────────


def test_reflection_compiles():
    result = compile_strategy(
        "reflection",
        {"pass_threshold": 0.75, "max_rounds": 2},
        ["search"],
        "gpt-4o",
        DEFAULT_LIMITS,
        "Write a thorough analysis",
        "agent-reflect",
    )
    nodes = result["nodes"]

    assert "__execute__" in nodes
    assert "__reflect_0__" in nodes
    assert "__reflect_gate_0__" in nodes
    assert "__revise_0__" in nodes
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    _assert_ir_invariants(result, "reflection")


def test_reflection_start_node():
    result = compile_strategy("reflection", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__execute__"


def test_reflection_max_rounds_capped():
    # max_rounds=10 but max_iterations=3 → only 3 reflect rounds
    limits = StrategyLimits(max_iterations=3, max_cost_usd=1.0, timeout_seconds=120)
    config = {"max_rounds": 10}
    result = compile_strategy("reflection", config, [], "gpt-4o", limits, "goal", "a")
    nodes = result["nodes"]
    assert "__reflect_2__" in nodes
    assert "__reflect_3__" not in nodes


def test_reflection_default_threshold():
    # config={} uses default pass_threshold (0.8 in the implementation)
    result = compile_strategy("reflection", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    # Verify reflection nodes still compile correctly with defaults
    nodes = result["nodes"]
    assert "__reflect_0__" in nodes
    assert "__execute__" in nodes


def test_reflection_invariants():
    result = compile_strategy("reflection", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    _assert_ir_invariants(result, "reflection")


# ── consensus ──────────────────────────────────────────────────────────────


def test_consensus_compiles():
    result = compile_strategy(
        "consensus",
        {"num_agents": 3},
        [],
        "gpt-4o",
        DEFAULT_LIMITS,
        "Answer this question",
        "agent-consensus",
    )
    nodes = result["nodes"]

    assert "__agent_0__" in nodes
    assert "__agent_1__" in nodes
    assert "__agent_2__" in nodes
    assert "__vote__" in nodes
    assert "__judge__" in nodes
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    _assert_ir_invariants(result, "consensus")


def test_consensus_start_node():
    result = compile_strategy("consensus", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__agent_0__"


def test_consensus_num_agents_config():
    config = {"num_agents": 5}
    result = compile_strategy("consensus", config, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    nodes = result["nodes"]
    assert "__agent_4__" in nodes
    assert "__agent_5__" not in nodes


def test_consensus_custom_judge_model():
    config = {"judge_model": "gpt-4o"}
    result = compile_strategy("consensus", config, [], "claude-sonnet-4-6", DEFAULT_LIMITS, "goal", "a")
    judge_node = result["nodes"]["__judge__"]
    assert judge_node["kind"]["model_ref"] == "gpt-4o"


def test_consensus_invariants():
    result = compile_strategy("consensus", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    _assert_ir_invariants(result, "consensus")


# ── debate ─────────────────────────────────────────────────────────────────


def test_debate_compiles():
    result = compile_strategy(
        "debate",
        {"max_rounds": 2},
        [],
        "gpt-4o",
        DEFAULT_LIMITS,
        "Determine the best approach",
        "agent-debate",
    )
    nodes = result["nodes"]

    assert "__propose__" in nodes
    assert "__counter_0__" in nodes
    assert "__judge_0__" in nodes
    assert "__respond_0__" in nodes
    assert "__finalize__" in nodes
    assert "__limit_exceeded__" in nodes

    _assert_ir_invariants(result, "debate")


def test_debate_start_node():
    result = compile_strategy("debate", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    assert result["start_node"] == "__propose__"


def test_debate_max_rounds_capped():
    # max_rounds=10 but max_iterations=3 → only 3 judge rounds
    limits = StrategyLimits(max_iterations=3, max_cost_usd=1.0, timeout_seconds=120)
    config = {"max_rounds": 10}
    result = compile_strategy("debate", config, [], "gpt-4o", limits, "goal", "a")
    nodes = result["nodes"]
    assert "__judge_2__" in nodes
    assert "__judge_3__" not in nodes


def test_debate_custom_judge_model():
    config = {"judge_model": "claude-opus-4-6"}
    result = compile_strategy("debate", config, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    judge_node = result["nodes"]["__judge_0__"]
    assert judge_node["kind"]["model_ref"] == "claude-opus-4-6"


def test_debate_invariants():
    result = compile_strategy("debate", {}, [], "gpt-4o", DEFAULT_LIMITS, "goal", "a")
    _assert_ir_invariants(result, "debate")


# ── Agent-first YAML round-trip (new strategies) ─────────────────────────


REFLECTION_YAML = """
agent:
  id: reflect-agent
  strategy: reflection
  goal: Write a thorough code review
  model: gpt-4o
  limits:
    max_iterations: 3
    max_cost_usd: 0.50
    timeout_seconds: 120
"""


def test_agent_yaml_reflection():
    ir = compile_yaml(REFLECTION_YAML)
    assert ir["workflow_id"] == "reflect-agent"
    assert ir["start_node"] == "__execute__"
    assert "__execute__" in ir["nodes"]
    assert "__reflect_0__" in ir["nodes"]
    assert ir["strategy_metadata"]["strategy_name"] == "reflection"
    assert ir["labels"]["jamjet.strategy"] == "reflection"


CONSENSUS_YAML = """
agent:
  id: consensus-agent
  strategy: consensus
  goal: Answer a disputed question
  model: gpt-4o
  strategy_config:
    num_agents: 4
  limits:
    max_iterations: 5
    max_cost_usd: 1.00
    timeout_seconds: 300
"""


def test_agent_yaml_consensus():
    ir = compile_yaml(CONSENSUS_YAML)
    assert ir["workflow_id"] == "consensus-agent"
    assert ir["start_node"] == "__agent_0__"
    assert "__agent_0__" in ir["nodes"]
    assert "__vote__" in ir["nodes"]
    assert "__judge__" in ir["nodes"]
    assert ir["strategy_metadata"]["strategy_name"] == "consensus"
    assert ir["labels"]["jamjet.strategy"] == "consensus"


DEBATE_YAML = """
agent:
  id: debate-agent
  strategy: debate
  goal: Determine the best programming language for AI
  model: gpt-4o
  limits:
    max_iterations: 4
    max_cost_usd: 0.80
    timeout_seconds: 240
"""


def test_agent_yaml_debate():
    ir = compile_yaml(DEBATE_YAML)
    assert ir["workflow_id"] == "debate-agent"
    assert ir["start_node"] == "__propose__"
    assert "__propose__" in ir["nodes"]
    assert "__counter_0__" in ir["nodes"]
    assert "__judge_0__" in ir["nodes"]
    assert ir["strategy_metadata"]["strategy_name"] == "debate"
    assert ir["labels"]["jamjet.strategy"] == "debate"
