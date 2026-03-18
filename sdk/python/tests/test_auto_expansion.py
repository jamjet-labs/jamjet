from jamjet.workflow.graph import WorkflowGraph
from jamjet.workflow.nodes import ModelNode


class TestAutoExpansion:
    def test_auto_agent_tool_expands_to_coordinator_plus_agent_tool(self):
        graph = WorkflowGraph("test_pipeline")
        graph.add_node("start", ModelNode(model="gpt-4"))
        graph.add_agent_tool("classify", agent="auto", output_key="result", mode="sync")
        graph.add_edge("start", "classify")

        ir = graph.compile()
        nodes = ir["nodes"]

        # Should have 3 nodes: start, _coordinator_classify, classify
        assert len(nodes) == 3
        assert "start" in nodes
        assert "classify" in nodes

        coordinator_ids = [nid for nid, n in nodes.items() if n["kind"]["type"] == "coordinator"]
        assert len(coordinator_ids) == 1
        coord_id = coordinator_ids[0]
        assert coord_id == "_coordinator_classify"

        coord_kind = nodes[coord_id]["kind"]
        assert coord_kind["type"] == "coordinator"
        assert coord_kind["strategy"] == "default"

        classify_kind = nodes["classify"]["kind"]
        assert classify_kind["type"] == "agent_tool"
        assert "auto" not in str(classify_kind.get("agent", {}))

        edges = ir["edges"]
        edge_pairs = [(e["from"], e["to"]) for e in edges]
        assert ("start", "_coordinator_classify") in edge_pairs
        assert ("_coordinator_classify", "classify") in edge_pairs

    def test_explicit_agent_not_expanded(self):
        graph = WorkflowGraph("test")
        graph.add_agent_tool("classify", agent="jamjet://org/classifier", output_key="result")
        ir = graph.compile()
        assert len(ir["nodes"]) == 1

    def test_multiple_auto_agents_expand_independently(self):
        graph = WorkflowGraph("test")
        graph.add_agent_tool("step1", agent="auto", output_key="r1")
        graph.add_agent_tool("step2", agent="auto", output_key="r2")
        graph.add_edge("step1", "step2")
        ir = graph.compile()
        assert len(ir["nodes"]) == 4
        coord_nodes = [n for n in ir["nodes"].values() if n["kind"]["type"] == "coordinator"]
        assert len(coord_nodes) == 2

    def test_auto_expansion_preserves_start_node(self):
        graph = WorkflowGraph("test")
        graph.add_agent_tool("first", agent="auto", output_key="r")
        ir = graph.compile()
        assert ir["start_node"] == "_coordinator_first"

    def test_compile_idempotent(self):
        graph = WorkflowGraph("test")
        graph.add_agent_tool("x", agent="auto", output_key="r")
        ir1 = graph.compile()
        ir2 = graph.compile()
        assert len(ir1["nodes"]) == len(ir2["nodes"]) == 2
