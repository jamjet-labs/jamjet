package dev.jamjet;

import dev.jamjet.ir.IrValidator;
import dev.jamjet.ir.WorkflowIr;
import dev.jamjet.workflow.Workflow;
import org.junit.jupiter.api.Test;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class IrValidatorTest {

    record S(String value) {}

    // ── Valid workflows ───────────────────────────────────────────────────────

    @Test
    void validSimpleWorkflow() {
        var wf = Workflow.<S>builder("valid-wf")
                .version("1.0.0")
                .state(S.class)
                .step("step_a", s -> new S("a"))
                .step("step_b", s -> new S("b"))
                .build();

        var errors = IrValidator.validate(wf.compile());
        assertTrue(errors.isEmpty(), "Expected no errors but got: " + errors);
    }

    @Test
    void validSingleStepWorkflow() {
        var wf = Workflow.<S>builder("one-step")
                .version("0.1.0")
                .state(S.class)
                .step("only", s -> s)
                .build();

        var errors = IrValidator.validate(wf.compile());
        assertTrue(errors.isEmpty(), "Expected no errors but got: " + errors);
    }

    @Test
    void validateOrThrowDoesNotThrowForValidIr() {
        var wf = Workflow.<S>builder("valid")
                .version("2.0.0")
                .state(S.class)
                .step("a", s -> s)
                .build();

        assertDoesNotThrow(() -> IrValidator.validateOrThrow(wf.compile()));
    }

    // ── Metadata errors ──────────────────────────────────────────────────────

    @Test
    void emptyWorkflowId() {
        var ir = makeIr("", "0.1.0", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("workflow_id")));
    }

    @Test
    void invalidSemverMissingPart() {
        var ir = makeIr("test", "1.0", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("semver")));
    }

    @Test
    void invalidSemverNotNumeric() {
        var ir = makeIr("test", "1.x.0", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("not a valid integer")));
    }

    @Test
    void invalidSemverGarbage() {
        var ir = makeIr("test", "not-semver", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("semver")));
    }

    // ── Start node errors ────────────────────────────────────────────────────

    @Test
    void emptyStartNode() {
        var ir = makeIr("test", "0.1.0", "",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("start_node is empty")));
    }

    @Test
    void startNodeNotInNodes() {
        var ir = makeIr("test", "0.1.0", "missing",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("does not exist")));
    }

    // ── Edge errors ──────────────────────────────────────────────────────────

    @Test
    void unknownEdgeTarget() {
        var ir = makeIr("test", "0.1.0", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "nonexistent")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("nonexistent") && e.contains("does not exist")));
    }

    @Test
    void edgeToEndIsValid() {
        var ir = makeIr("test", "0.1.0", "a",
                Map.of("a", simpleNode("a")),
                List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.isEmpty(), "Edge to 'end' should be valid but got: " + errors);
    }

    // ── Reachability errors ──────────────────────────────────────────────────

    @Test
    void unreachableNode() {
        var nodes = new LinkedHashMap<String, Object>();
        nodes.put("a", simpleNode("a"));
        nodes.put("orphan", simpleNode("orphan"));

        var ir = makeIr("test", "0.1.0", "a", nodes, List.of(edge("a", "end")));

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("orphan") && e.contains("unreachable")),
                "Expected unreachable error for 'orphan' but got: " + errors);
    }

    @Test
    void allNodesReachable() {
        var nodes = new LinkedHashMap<String, Object>();
        nodes.put("a", simpleNode("a"));
        nodes.put("b", simpleNode("b"));
        nodes.put("c", simpleNode("c"));

        var edges = List.of(edge("a", "b"), edge("b", "c"), edge("c", "end"));
        var ir = makeIr("test", "0.1.0", "a", nodes, edges);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.isEmpty(), "All nodes should be reachable but got: " + errors);
    }

    // ── Reference errors ─────────────────────────────────────────────────────

    @Test
    void unknownToolRef() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "tool");
        kind.put("tool_ref", "missing_tool");
        var node = new LinkedHashMap<String, Object>();
        node.put("id", "a");
        node.put("kind", kind);
        node.put("labels", Map.of());

        // tools map has an entry but not "missing_tool" — triggers ref validation
        var ir = new WorkflowIr("test", "0.1.0", null, null, "",
                "a", Map.of("a", node), List.of(edge("a", "end")),
                Map.of(), Map.of(), Map.of(),
                Map.of("other_tool", Map.of("name", "other_tool")),
                Map.of(), Map.of(), Map.of(), null);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("missing_tool") && e.contains("unknown tool")));
    }

    @Test
    void unknownModelRef() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "model");
        kind.put("model_ref", "missing_model");
        var node = new LinkedHashMap<String, Object>();
        node.put("id", "a");
        node.put("kind", kind);
        node.put("labels", Map.of());

        // models map has an entry but not "missing_model"
        var ir = new WorkflowIr("test", "0.1.0", null, null, "",
                "a", Map.of("a", node), List.of(edge("a", "end")),
                Map.of(), Map.of(),
                Map.of("other_model", Map.of("name", "other_model")),
                Map.of(), Map.of(), Map.of(), Map.of(), null);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("missing_model") && e.contains("unknown model")));
    }

    @Test
    void unknownMcpServer() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "mcp_tool");
        kind.put("server", "missing_server");
        var node = new LinkedHashMap<String, Object>();
        node.put("id", "a");
        node.put("kind", kind);
        node.put("labels", Map.of());

        // mcp_servers map has an entry but not "missing_server"
        var ir = new WorkflowIr("test", "0.1.0", null, null, "",
                "a", Map.of("a", node), List.of(edge("a", "end")),
                Map.of(), Map.of(), Map.of(), Map.of(),
                Map.of("other_server", Map.of("url", "http://other")),
                Map.of(), Map.of(), null);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("missing_server") && e.contains("MCP server")));
    }

    @Test
    void unknownRemoteAgent() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "a2a_task");
        kind.put("remote_agent", "missing_agent");
        var node = new LinkedHashMap<String, Object>();
        node.put("id", "a");
        node.put("kind", kind);
        node.put("labels", Map.of());

        // remote_agents map has an entry but not "missing_agent"
        var ir = new WorkflowIr("test", "0.1.0", null, null, "",
                "a", Map.of("a", node), List.of(edge("a", "end")),
                Map.of(), Map.of(), Map.of(), Map.of(), Map.of(),
                Map.of("other_agent", Map.of("url", "http://other")),
                Map.of(), null);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.stream().anyMatch(e -> e.contains("missing_agent") && e.contains("remote agent")));
    }

    @Test
    void validToolRefPasses() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "tool");
        kind.put("tool_ref", "my_tool");
        var node = new LinkedHashMap<String, Object>();
        node.put("id", "a");
        node.put("kind", kind);
        node.put("labels", Map.of());

        var ir = new WorkflowIr("test", "0.1.0", null, null, "",
                "a", Map.of("a", node), List.of(edge("a", "end")),
                Map.of(), Map.of(), Map.of(),
                Map.of("my_tool", Map.of("name", "my_tool")), // tool exists
                Map.of(), Map.of(), Map.of(), null);

        var errors = IrValidator.validate(ir);
        assertTrue(errors.isEmpty(), "Valid tool ref should pass but got: " + errors);
    }

    // ── Multiple errors ──────────────────────────────────────────────────────

    @Test
    void multipleErrorsCollected() {
        var ir = makeIr("", "bad", "", Map.of(), List.of());

        var errors = IrValidator.validate(ir);
        assertTrue(errors.size() >= 2, "Expected multiple errors but got " + errors.size() + ": " + errors);
    }

    @Test
    void validateOrThrowThrowsWithMessage() {
        var ir = makeIr("", "0.1.0", "a", Map.of(), List.of());

        var ex = assertThrows(IllegalStateException.class, () -> IrValidator.validateOrThrow(ir));
        assertTrue(ex.getMessage().contains("validation failed"));
    }

    // ── Agent IR validation ──────────────────────────────────────────────────

    @Test
    void agentCompiledIrPassesValidation() {
        var agent = dev.jamjet.agent.Agent.builder("validator-test")
                .model("gpt-4o")
                .strategy("react")
                .maxIterations(3)
                .build();

        var errors = IrValidator.validate(agent.compile());
        assertTrue(errors.isEmpty(), "Agent-compiled IR should be valid but got: " + errors);
    }

    @Test
    void criticAgentIrPassesValidation() {
        var agent = dev.jamjet.agent.Agent.builder("critic-test")
                .model("gpt-4o")
                .strategy("critic")
                .maxIterations(5)
                .build();

        var errors = IrValidator.validate(agent.compile());
        assertTrue(errors.isEmpty(), "Critic IR should be valid but got: " + errors);
    }

    @Test
    void planAndExecuteIrPassesValidation() {
        var agent = dev.jamjet.agent.Agent.builder("plan-test")
                .model("gpt-4o")
                .strategy("plan-and-execute")
                .maxIterations(5)
                .build();

        var errors = IrValidator.validate(agent.compile());
        assertTrue(errors.isEmpty(), "Plan-and-execute IR should be valid but got: " + errors);
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    private static WorkflowIr makeIr(String id, String version, String startNode,
                                     Map<String, Object> nodes, List<Map<String, Object>> edges) {
        return new WorkflowIr(id, version, null, null, "",
                startNode, nodes, edges,
                Map.of(), Map.of(), Map.of(), Map.of(), Map.of(), Map.of(), Map.of(), null);
    }

    private static Map<String, Object> simpleNode(String id) {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "condition");
        kind.put("branches", List.of());
        var node = new LinkedHashMap<String, Object>();
        node.put("id", id);
        node.put("kind", kind);
        node.put("labels", Map.of());
        return node;
    }

    private static Map<String, Object> edge(String from, String to) {
        var e = new LinkedHashMap<String, Object>();
        e.put("from", from);
        e.put("to", to);
        e.put("condition", null);
        return e;
    }
}
