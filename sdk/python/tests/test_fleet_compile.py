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
