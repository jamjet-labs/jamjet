from jamjet.spec import EdgeSpec, NodeSpec, WorkflowSpec


def test_minimal_workflow():
    w = WorkflowSpec(
        name="trip",
        nodes=[NodeSpec(id="a", handler_ref="m:f"), NodeSpec(id="b", handler_ref="m:g")],
        edges=[EdgeSpec(from_node="a", to_node="b")],
        entry_node="a",
    )
    assert w.kind == "workflow"
    assert w.entry_node == "a"


def test_edge_optional_condition():
    e = EdgeSpec(from_node="a", to_node="b", condition="is_ready")
    assert e.condition == "is_ready"


def test_round_trip_json():
    w = WorkflowSpec(
        name="x",
        nodes=[NodeSpec(id="a", handler_ref="m:f")],
        edges=[],
        entry_node="a",
    )
    assert WorkflowSpec.model_validate_json(w.model_dump_json()) == w
