from jamjet.workflow.ir_compiler import _compile_graph_yaml


def test_compile_graph_yaml_builds_ir_from_a_graph_doc():
    data = {
        "workflow": {"id": "etl", "version": "0.1.0", "start": "extract"},
        "nodes": {
            "extract": {"type": "tool", "tool_ref": "pull", "next": "end"},
        },
    }
    ir = _compile_graph_yaml(data)
    assert ir["workflow_id"] == "etl"
    assert ir["version"] == "0.1.0"
    assert ir["start_node"] == "extract"
    assert "extract" in ir["nodes"]
    assert {"from": "extract", "to": "end", "condition": None} in ir["edges"]


from jamjet.workflow.bundle import CompiledBundle, CronSpec, compile_bundle, is_bundle


def test_is_bundle_detects_plural_maps():
    assert is_bundle({"agents": {}}) is True
    assert is_bundle({"workflows": {}}) is True
    assert is_bundle({"agent": {"id": "x"}}) is False
    assert is_bundle({"workflow": {"id": "x"}, "nodes": {}}) is False


def test_empty_bundle_returns_empty_lists():
    bundle = compile_bundle({"agents": {}})
    assert isinstance(bundle, CompiledBundle)
    assert bundle.workflows == []
    assert bundle.cron_jobs == []


import pytest
from jamjet.workflow.bundle import _validate_cron, _schedule_to_spec


def test_validate_cron_accepts_five_fields():
    _validate_cron("0 9 * * *")  # no raise


def test_validate_cron_rejects_wrong_field_count():
    with pytest.raises(ValueError, match="5 fields"):
        _validate_cron("* * * *")


def test_schedule_to_spec_defaults_and_utc():
    spec = _schedule_to_spec("researcher", "0.1.0", {"cron": "0 9 * * *"})
    assert spec.name == "researcher"
    assert spec.workflow_id == "researcher"
    assert spec.workflow_version == "0.1.0"
    assert spec.enabled is True
    assert spec.input == {}


def test_schedule_to_spec_rejects_non_utc():
    with pytest.raises(ValueError, match="UTC"):
        _schedule_to_spec("x", "0.1.0", {"cron": "0 9 * * *", "timezone": "America/New_York"})


from jamjet.workflow.bundle import _resolve_uses


def test_resolve_uses_splits_tools_mcp_and_siblings():
    tool_catalog = {"web_search": {"description": "search", "input_schema": {}}}
    mcp_catalog = {"github": {"url": "x", "transport": "streamable-http"}}
    unit_ids = {"researcher", "reconciler"}
    resolved = _resolve_uses(
        unit_id="reconciler",
        uses=["tool:web_search", "mcp:github", "agent:researcher"],
        inline_tools=[{"name": "post", "description": "post", "input_schema": {}}],
        tool_catalog=tool_catalog,
        mcp_catalog=mcp_catalog,
        unit_ids=unit_ids,
    )
    assert set(resolved.tool_names) == {"web_search", "post", "researcher"}
    assert "github" in resolved.mcp_servers
    assert "web_search" in resolved.ir_tools
    assert "post" in resolved.ir_tools
    assert resolved.sibling_refs == ["researcher"]


def test_resolve_uses_unknown_tool_errors():
    with pytest.raises(ValueError, match="unknown tool 'nope'"):
        _resolve_uses("a", ["tool:nope"], [], {}, {}, {"a"})


def test_resolve_uses_unknown_sibling_errors():
    with pytest.raises(ValueError, match="unknown unit 'ghost'"):
        _resolve_uses("a", ["agent:ghost"], [], {}, {}, {"a"})


def test_resolve_uses_bad_prefix_errors():
    with pytest.raises(ValueError, match="unknown ref"):
        _resolve_uses("a", ["banana:x"], [], {}, {}, {"a"})


def test_compile_agent_unit_builds_ir_and_populates_mcp():
    tool_catalog = {"web_search": {"description": "s", "input_schema": {}}}
    mcp_catalog = {"github": {"url": "u", "transport": "streamable-http"}}
    from jamjet.workflow.bundle import _compile_agent_unit
    ir = _compile_agent_unit(
        unit_id="researcher",
        agent={
            "strategy": "plan-and-execute",
            "goal": "summarize",
            "uses": ["tool:web_search", "mcp:github"],
            "limits": {"max_iterations": 3, "max_cost_usd": 0.5, "timeout_seconds": 60},
        },
        defaults={"model": "claude-sonnet-4-6"},
        tool_catalog=tool_catalog,
        mcp_catalog=mcp_catalog,
        unit_ids={"researcher"},
    )
    assert ir["workflow_id"] == "researcher"
    assert ir["version"] == "0.1.0"
    assert ir["mcp_servers"] == {"github": mcp_catalog["github"]}
    assert "web_search" in ir["tools"]
    assert ir["labels"]["jamjet.agent.id"] == "researcher"
    assert ir["nodes"]  # strategy produced nodes


def test_compile_agent_unit_requires_goal():
    from jamjet.workflow.bundle import _compile_agent_unit
    with pytest.raises(ValueError, match="goal"):
        _compile_agent_unit("x", {"strategy": "react", "limits": {
            "max_iterations": 1, "max_cost_usd": 0.1, "timeout_seconds": 10}},
            {"model": "m"}, {}, {}, {"x"})
