package dev.jamjet;

import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.agent.Agent;
import dev.jamjet.ir.WorkflowIr;
import dev.jamjet.workflow.Workflow;
import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class IrCompilerTest {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    record SimpleState(String value) {}

    @Test
    void workflowIrJsonRoundTrip() {
        var wf = Workflow.<SimpleState>builder("rt-test")
                .version("2.0.0")
                .state(SimpleState.class)
                .step("step_a", s -> new SimpleState(s.value() + "_a"))
                .step("step_b", s -> new SimpleState(s.value() + "_b"))
                .build();

        var ir = wf.compile();
        var json = ir.toJson();
        assertNotNull(json);
        assertFalse(json.isBlank());

        var restored = WorkflowIr.fromJson(json);
        assertEquals(ir.id(), restored.id());
        assertEquals(ir.version(), restored.version());
        assertEquals(ir.startNode(), restored.startNode());
        assertEquals(ir.nodes().size(), restored.nodes().size());
        assertEquals(ir.edges().size(), restored.edges().size());
    }

    @Test
    void irJsonContainsSnakeCaseFields() throws Exception {
        var wf = Workflow.<SimpleState>builder("snake-case-test")
                .state(SimpleState.class)
                .step("my_step", s -> s)
                .build();

        var json = wf.compile().toJson();
        var tree = MAPPER.readTree(json);

        // Key fields must be snake_case
        assertTrue(tree.has("workflow_id"), "Should have workflow_id");
        assertTrue(tree.has("start_node"), "Should have start_node");
        assertTrue(tree.has("state_schema"), "Should have state_schema");
        assertTrue(tree.has("retry_policies"), "Should have retry_policies");
        assertTrue(tree.has("mcp_servers"), "Should have mcp_servers");
        assertTrue(tree.has("remote_agents"), "Should have remote_agents");

        // Must not have camelCase variants
        assertFalse(tree.has("workflowId"), "Should NOT have workflowId");
        assertFalse(tree.has("startNode"), "Should NOT have startNode");
    }

    @Test
    void irEdgesHaveCorrectFromToConditionStructure() throws Exception {
        var wf = Workflow.<SimpleState>builder("edge-test")
                .state(SimpleState.class)
                .step("first", s -> s)
                .step("second", s -> s)
                .build();

        var json = wf.compile().toJson();
        var tree = MAPPER.readTree(json);
        var edges = tree.get("edges");
        assertNotNull(edges);
        assertTrue(edges.isArray());
        assertTrue(edges.size() > 0);

        var edge0 = edges.get(0);
        assertTrue(edge0.has("from"));
        assertTrue(edge0.has("to"));
        assertTrue(edge0.has("condition"));
        assertEquals("first", edge0.get("from").asText());
        assertEquals("second", edge0.get("to").asText());
    }

    @Test
    void agentIrNodesHaveIdAndKindAndLabels() throws Exception {
        var agent = Agent.builder("ir-check-agent")
                .model("gpt-4o")
                .strategy("react")
                .maxIterations(2)
                .build();

        var ir = agent.compile();
        var json = ir.toJson();
        var tree = MAPPER.readTree(json);
        var nodes = tree.get("nodes");
        assertNotNull(nodes);

        nodes.fields().forEachRemaining(entry -> {
            var node = entry.getValue();
            assertTrue(node.has("id"), "Node should have 'id': " + entry.getKey());
            assertTrue(node.has("kind"), "Node should have 'kind': " + entry.getKey());
            assertTrue(node.has("labels"), "Node should have 'labels': " + entry.getKey());
        });
    }

    @Test
    void agentIrHasStrategyMetadata() throws Exception {
        var agent = Agent.builder("meta-check")
                .model("gpt-4o")
                .strategy("critic")
                .maxIterations(2)
                .build();

        var json = agent.compile().toJson();
        var tree = MAPPER.readTree(json);
        assertTrue(tree.has("strategy_metadata"), "Should have strategy_metadata");
        var meta = tree.get("strategy_metadata");
        assertEquals("critic", meta.get("strategy_name").asText());
        assertEquals("meta-check", meta.get("agent_id").asText());
        assertTrue(meta.has("limits"));
    }

    @Test
    void workflowIrToMapIsSerializable() throws Exception {
        var wf = Workflow.<SimpleState>builder("map-test")
                .state(SimpleState.class)
                .step("step_x", s -> s)
                .build();

        var ir = wf.compile();
        var map = ir.toMap();
        assertNotNull(map);
        assertTrue(map.containsKey("workflow_id"));
        assertTrue(map.containsKey("start_node"));
        assertTrue(map.containsKey("nodes"));
        assertTrue(map.containsKey("edges"));

        // Should be re-serializable to JSON
        var json = MAPPER.writeValueAsString(map);
        assertFalse(json.isBlank());
    }

    @Test
    void reactIrStartNodeIsThink0() {
        var agent = Agent.builder("react-start")
                .model("gpt-4o")
                .strategy("react")
                .maxIterations(3)
                .build();

        var ir = agent.compile();
        assertEquals("__think_0__", ir.startNode());
    }

    @Test
    void planAndExecuteIrStartNodeIsPlan() {
        var agent = Agent.builder("plan-start")
                .model("gpt-4o")
                .strategy("plan-and-execute")
                .maxIterations(3)
                .build();

        var ir = agent.compile();
        assertEquals("__plan__", ir.startNode());
    }

    @Test
    void criticIrStartNodeIsDraft() {
        var agent = Agent.builder("critic-start")
                .model("gpt-4o")
                .strategy("critic")
                .maxIterations(3)
                .build();

        var ir = agent.compile();
        assertEquals("__draft__", ir.startNode());
    }
}
