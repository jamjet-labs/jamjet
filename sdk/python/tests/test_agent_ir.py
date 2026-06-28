"""Structural tests for the agent-loop IR builder (Track 2j-3 + T3-5).

No engine: assert the compiled ``WorkflowIr`` dict has the model/condition/
tool-dispatch nodes, the correct edges (model -> gate -> (tools -> next | end)),
and the canonical top-level shape the Rust engine deserializes.

T3-5 section (at the bottom) verifies that governance knobs from GovernanceConfig
compile into the correct IR fields:
  budget.cost_usd  -> cost_budget_usd
  budget.tokens    -> token_budget.total_tokens
  policy/approval_required -> policy.require_approval_for
  pii=True (default) -> data_policy with standard PII detectors
"""

from __future__ import annotations

import pytest

from jamjet.agents.agent import Agent
from jamjet.agents.governance import Budget
from jamjet.compiler.agent_ir import build_initial_state, compile_agent_to_ir
from jamjet.tools.decorators import tool


@tool
async def get_weather(city: str) -> str:
    """Return the weather for a city."""
    return f"sunny in {city}"


@tool
async def search_web(query: str, limit: int = 5) -> str:
    """Search the web."""
    return f"results for {query} (limit {limit})"


def _agent() -> Agent:
    return Agent(
        "research",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather, search_web],
        instructions="You are a research assistant.",
    )


# ── Top-level shape ───────────────────────────────────────────────────────────

# The canonical WorkflowIr keys produced by jamjet.workflow.ir_compiler — the
# shape the Rust engine deserializes (there is no Python validator for the
# kind-based IR, so we assert required keys + node/edge invariants).
_REQUIRED_KEYS = {
    "workflow_id",
    "version",
    "name",
    "description",
    "state_schema",
    "start_node",
    "nodes",
    "edges",
    "retry_policies",
    "timeouts",
    "models",
    "tools",
    "mcp_servers",
    "remote_agents",
    "labels",
}


def test_ir_has_required_top_level_keys():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    assert _REQUIRED_KEYS <= set(ir), f"missing: {_REQUIRED_KEYS - set(ir)}"
    assert ir["start_node"] == "__model_0__"
    assert ir["start_node"] in ir["nodes"]
    assert ir["workflow_id"] == "research"


def test_node_and_edge_invariants():
    """Every node is normalised; every edge target is a known node or `end`."""
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    node_ids = set(ir["nodes"]) | {"end"}
    for node_id, node in ir["nodes"].items():
        assert node["id"] == node_id
        assert "kind" in node and "type" in node["kind"]
        assert {"retry_policy", "node_timeout_secs", "description", "labels"} <= set(node)
    for e in ir["edges"]:
        assert e["from"] in ir["nodes"], f"edge from unknown node: {e['from']}"
        assert e["to"] in node_ids, f"edge to unknown node: {e['to']}"


# ── Model nodes ───────────────────────────────────────────────────────────────


def test_turn_model_nodes_each_carry_both_tool_schemas():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    # The per-turn model nodes (0..max_turns-1) carry the tool schemas.
    model_ids = [f"__model_{t}__" for t in range(3)]
    assert all(mid in ir["nodes"] for mid in model_ids)
    for mid in model_ids:
        kind = ir["nodes"][mid]["kind"]
        assert kind["type"] == "model"
        assert kind["model_ref"] == "anthropic/claude-sonnet-4-6"
        assert kind["system_prompt"] == "You are a research assistant."
        # Empty prompt_ref => the executor reads `messages` from state.
        assert kind["prompt_ref"] == ""
        names = [s["function"]["name"] for s in kind["tools"]]
        assert names == ["get_weather", "search_web"]
        # OpenAI function-schema shape; parameters are the tool's input_schema
        # verbatim (this SDK's @tool maps a str param to the bare token "string").
        assert kind["tools"][0]["type"] == "function"
        params = kind["tools"][0]["function"]["parameters"]
        assert params["type"] == "object"
        assert params["properties"]["city"] == "string"
        assert params["required"] == ["city"]


def test_final_model_node_carries_no_tools_and_ends():
    """The final model node (index == max_turns) consumes the last tool results and
    must answer (no tool schemas), and is the only node routing to `end` besides the
    per-turn gates — the terminal is always reached via a model node."""
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    final = ir["nodes"]["__model_3__"]
    assert final["kind"]["type"] == "model"
    assert final["kind"]["tools"] == [], "final model must offer no tools so it answers"
    assert final["labels"].get("jamjet.agent.final") == "true"
    assert ("__model_3__", "end") in _edge_set(ir)
    # No node with a tool-dispatch kind routes directly to `end`.
    tool_ids = {nid for nid, n in ir["nodes"].items() if n["kind"]["type"] == "python_fn"}
    assert not any(frm in tool_ids and to == "end" for frm, to in _edge_set(ir))


# ── Condition gates ───────────────────────────────────────────────────────────


def test_three_condition_gates_branch_on_tool_calls():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    expr = 'state.last_model_finish_reason == "tool_calls"'
    for t in range(3):
        kind = ir["nodes"][f"__tool_gate_{t}__"]["kind"]
        assert kind["type"] == "condition"
        branches = kind["branches"]
        # true branch -> the turn's tool node; default branch -> terminal.
        assert branches[0] == {"condition": expr, "target": f"__tools_{t}__"}
        assert branches[1] == {"condition": None, "target": "end"}


# ── Tool-dispatch PythonFn nodes ──────────────────────────────────────────────


def test_three_tool_dispatch_nodes_with_resolver_map():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    for t in range(3):
        kind = ir["nodes"][f"__tools_{t}__"]["kind"]
        assert kind["type"] == "python_fn"
        assert kind["module"] == "jamjet.agents.tool_runtime"
        assert kind["function"] == "dispatch_tool_calls"
        tools_map = kind["input"]["tools"]
        assert set(tools_map) == {"get_weather", "search_web"}
        # name -> "module:qualname" (mirrors Agent.compile handler_ref).
        assert tools_map["get_weather"].endswith(":get_weather")
        assert tools_map["search_web"].endswith(":search_web")
        assert ":" in tools_map["get_weather"]
        # The dispatch input references the 2j-2 state keys.
        assert kind["input"]["tool_calls"] == "$state.last_model_tool_calls"
        assert kind["input"]["assistant_content"] == "$state.last_model_output"
        assert kind["input"]["messages"] == "$state.messages"


# ── Edges / loop topology ─────────────────────────────────────────────────────


def _edge_set(ir: dict) -> set[tuple[str, str]]:
    return {(e["from"], e["to"]) for e in ir["edges"]}


def test_loop_edges_route_model_gate_tools_next():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    edges = _edge_set(ir)
    # model -> gate, gate -> tools, gate -> end for every turn.
    for t in range(3):
        assert (f"__model_{t}__", f"__tool_gate_{t}__") in edges
        assert (f"__tool_gate_{t}__", f"__tools_{t}__") in edges
        assert (f"__tool_gate_{t}__", "end") in edges
    # Every tools node loops forward to the NEXT model node — including the last
    # turn, whose dispatch flows into the final model node (never directly to end).
    assert (f"__tools_{0}__", "__model_1__") in edges
    assert (f"__tools_{1}__", "__model_2__") in edges
    assert ("__tools_2__", "__model_3__") in edges
    assert ("__tools_2__", "end") not in edges
    # The final model node is the bounded terminal model -> end, and has no gate.
    assert "__model_3__" in ir["nodes"]
    assert ("__model_3__", "end") in edges
    assert "__tool_gate_3__" not in ir["nodes"]


def test_terminal_is_reachable_end_sentinel():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=2)
    targets = {e["to"] for e in ir["edges"]}
    assert "end" in targets
    assert "end" not in ir["nodes"]  # `end` is the graph terminal sentinel


def test_node_counts_scale_with_max_turns():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=4)
    kinds = [n["kind"]["type"] for n in ir["nodes"].values()]
    # max_turns turn-models + 1 final model; one gate and one dispatch per turn.
    assert kinds.count("model") == 5
    assert kinds.count("condition") == 4
    assert kinds.count("python_fn") == 4


# ── max_turns=1 + validation ──────────────────────────────────────────────────


def test_single_turn_dispatch_routes_to_final_model_then_end():
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=1)
    # One turn (model_0/gate_0/tools_0) plus the final model node (model_1).
    assert set(ir["nodes"]) == {"__model_0__", "__tool_gate_0__", "__tools_0__", "__model_1__"}
    edges = _edge_set(ir)
    # The single dispatch routes into the final model, which routes to end.
    assert ("__tools_0__", "__model_1__") in edges
    assert ("__model_1__", "end") in edges
    assert ("__tools_0__", "end") not in edges


def test_max_turns_must_be_positive():
    with pytest.raises(ValueError, match="max_turns"):
        compile_agent_to_ir(_agent(), "hi", max_turns=0)


# ── Initial state ─────────────────────────────────────────────────────────────


def test_build_initial_state_seeds_messages_and_tools():
    state = build_initial_state(_agent(), "what's the weather?")
    assert state["messages"][0] == {"role": "system", "content": "You are a research assistant."}
    assert state["messages"][1] == {"role": "user", "content": "what's the weather?"}
    assert set(state["tools"]) == {"get_weather", "search_web"}
    assert state["tools"]["get_weather"].endswith(":get_weather")


# ── Retry policy (tool-dispatch nodes must not auto-retry) ─────────────────────


def test_tool_dispatch_nodes_disable_retry():
    """The __tools__ nodes run non-idempotent @tool functions, so they must carry a
    no-retry policy (scheduler resolves no_retry -> max_attempts 1); model nodes keep
    the rate-limit-friendly llm_default."""
    ir = compile_agent_to_ir(_agent(), "hi", max_turns=3)
    for t in range(3):
        assert ir["nodes"][f"__tools_{t}__"]["retry_policy"] == "no_retry"
        assert ir["nodes"][f"__model_{t}__"]["retry_policy"] == "llm_default"
    assert ir["nodes"]["__model_3__"]["retry_policy"] == "llm_default"


# ── Content-derived version + stable description (cache key + privacy) ─────────


def test_version_is_content_derived_and_prompt_independent():
    base = compile_agent_to_ir(_agent(), "prompt A", max_turns=3)["version"]
    assert base.startswith("0.1.0+")
    # Same agent + same max_turns => same cache key, regardless of the prompt.
    assert compile_agent_to_ir(_agent(), "a different prompt", max_turns=3)["version"] == base
    # A changed max_turns changes the cache key (a different graph).
    assert compile_agent_to_ir(_agent(), "prompt A", max_turns=4)["version"] != base
    # Changed instructions => changed key.
    other = Agent(
        "research",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather, search_web],
        instructions="You are a DIFFERENT assistant.",
    )
    assert compile_agent_to_ir(other, "prompt A", max_turns=3)["version"] != base


def test_description_is_stable_and_never_the_prompt():
    prompt = "this prompt must not leak into the workflow definition"
    ir = compile_agent_to_ir(_agent(), prompt, max_turns=2)
    # Instructions are set, so the description is the instructions (stable) — never the prompt.
    assert ir["description"] == "You are a research assistant."
    assert prompt not in ir["description"]
    # With empty instructions, the description is a stable agent-name string, not the prompt.
    bare = Agent("bare", model="anthropic/claude-sonnet-4-6", tools=[get_weather])
    ir_bare = compile_agent_to_ir(bare, prompt, max_turns=2)
    assert ir_bare["description"] == "Durable agent: bare"
    assert prompt not in ir_bare["description"]


# ── Governance IR (T3-5) ──────────────────────────────────────────────────────
#
# Verify that GovernanceConfig knobs compile into the correct Rust WorkflowIr
# fields.  Field names must match workflow.rs exactly (serde snake_case):
#   cost_budget_usd           -> Option<f64>
#   token_budget.total_tokens -> Option<u32>  (inside TokenBudgetIr)
#   policy.require_approval_for -> Vec<String> (inside PolicySetIr)
#   data_policy.pii_detectors   -> Vec<String> (inside DataPolicyIr)


def _governed_agent(**kwargs) -> Agent:
    """Build a minimal Agent with governance knobs for IR tests."""
    return Agent(
        "governed",
        model="anthropic/claude-sonnet-4-6",
        tools=[get_weather],
        **kwargs,
    )


class TestGovernanceIrBudget:
    """budget= knob compiles to cost_budget_usd / token_budget IR fields."""

    def test_float_budget_emits_cost_budget_usd(self):
        ir = compile_agent_to_ir(_governed_agent(budget=0.50), "hi")
        assert ir.get("cost_budget_usd") == 0.50
        assert "token_budget" not in ir

    def test_int_budget_emits_cost_budget_usd(self):
        ir = compile_agent_to_ir(_governed_agent(budget=2), "hi")
        assert ir.get("cost_budget_usd") == 2.0
        assert "token_budget" not in ir

    def test_budget_tokens_emits_token_budget(self):
        ir = compile_agent_to_ir(_governed_agent(budget=Budget(tokens=1000)), "hi")
        assert "cost_budget_usd" not in ir
        assert ir.get("token_budget") == {"total_tokens": 1000}

    def test_budget_both_fields_emits_both_ir_keys(self):
        ir = compile_agent_to_ir(_governed_agent(budget=Budget(tokens=5000, cost_usd=1.00)), "hi")
        assert ir.get("cost_budget_usd") == 1.00
        assert ir.get("token_budget") == {"total_tokens": 5000}

    def test_no_budget_emits_no_budget_fields(self):
        """A plain Agent() must not produce budget IR — a zero-value would deny all runs."""
        ir = compile_agent_to_ir(_agent(), "hi")
        assert "cost_budget_usd" not in ir
        assert "token_budget" not in ir


class TestGovernanceIrPolicy:
    """policy= and approval_required= knobs compile to the PolicySetIr block."""

    def test_approval_required_list_emits_require_approval_for(self):
        ir = compile_agent_to_ir(
            _governed_agent(policy="strict", approval_required=["delete_*"], budget=0.50),
            "hi",
        )
        # budget
        assert ir.get("cost_budget_usd") == 0.50
        # policy block with the glob
        policy = ir.get("policy")
        assert policy is not None, "policy block must be present"
        assert policy["require_approval_for"] == ["delete_*"]

    def test_approval_required_true_maps_to_wildcard(self):
        ir = compile_agent_to_ir(_governed_agent(approval_required=True), "hi")
        policy = ir.get("policy")
        assert policy is not None
        assert policy["require_approval_for"] == ["*"]

    def test_approval_required_multi_glob(self):
        globs = ["send_*", "transfer_*"]
        ir = compile_agent_to_ir(_governed_agent(approval_required=globs), "hi")
        policy = ir.get("policy")
        assert policy is not None
        assert policy["require_approval_for"] == globs

    def test_dict_policy_passes_through_blocked_tools_and_allowlist(self):
        p = {"blocked_tools": ["rm_*"], "model_allowlist": ["claude-*"]}
        ir = compile_agent_to_ir(_governed_agent(policy=p), "hi")
        policy = ir.get("policy")
        assert policy is not None
        assert policy["blocked_tools"] == ["rm_*"]
        assert policy["model_allowlist"] == ["claude-*"]
        assert policy["require_approval_for"] == []

    def test_dict_policy_and_approval_required_are_merged(self):
        """approval_required globs are union-appended into a dict policy's require_approval_for."""
        p = {"require_approval_for": ["pay_*"]}
        ir = compile_agent_to_ir(_governed_agent(policy=p, approval_required=["delete_*"]), "hi")
        policy = ir.get("policy")
        assert policy is not None
        assert "pay_*" in policy["require_approval_for"]
        assert "delete_*" in policy["require_approval_for"]

    def test_no_policy_no_approval_omits_policy_block(self):
        """A plain Agent() must not emit a policy block."""
        ir = compile_agent_to_ir(_agent(), "hi")
        assert "policy" not in ir

    def test_approval_required_false_and_no_policy_omits_policy_block(self):
        ir = compile_agent_to_ir(_governed_agent(approval_required=False), "hi")
        assert "policy" not in ir

    def test_empty_approval_list_omits_policy_block(self):
        ir = compile_agent_to_ir(_governed_agent(approval_required=[]), "hi")
        assert "policy" not in ir


class TestGovernanceIrDataPolicy:
    """pii= knob compiles to the DataPolicyIr block."""

    def test_default_pii_true_emits_data_policy(self):
        """pii=True (default) -> data_policy with the five standard detectors."""
        ir = compile_agent_to_ir(_agent(), "hi")
        dp = ir.get("data_policy")
        assert dp is not None, "data_policy must be present when pii=True"
        detectors = dp["pii_detectors"]
        for expected in ("email", "ssn", "credit_card", "phone", "ip_address"):
            assert expected in detectors, f"missing detector: {expected}"
        assert dp["redaction_mode"] == "mask"
        assert dp["retain_prompts"] is False
        assert dp["retain_outputs"] is True

    def test_explicit_pii_true_emits_data_policy(self):
        ir = compile_agent_to_ir(_governed_agent(pii=True), "hi")
        assert ir.get("data_policy") is not None

    def test_pii_false_omits_data_policy(self):
        """pii=False -> no data_policy emitted (no Rust-side PII redaction)."""
        ir = compile_agent_to_ir(_governed_agent(pii=False), "hi")
        assert "data_policy" not in ir


class TestGovernanceIrCacheKey:
    """Governance changes must produce a different content-version (cache key)."""

    def test_adding_budget_changes_version(self):
        base_version = compile_agent_to_ir(_agent(), "hi")["version"]
        budgeted_version = compile_agent_to_ir(_governed_agent(budget=0.50), "hi")["version"]
        assert base_version != budgeted_version

    def test_adding_approval_changes_version(self):
        base_version = compile_agent_to_ir(_agent(), "hi")["version"]
        approved_version = compile_agent_to_ir(_governed_agent(approval_required=["delete_*"]), "hi")["version"]
        assert base_version != approved_version
