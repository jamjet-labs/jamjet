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
