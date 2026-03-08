package dev.jamjet;

import dev.jamjet.ir.StrategyCompiler;
import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class StrategyCompilerTest {

    private static final List<String> TOOLS = List.of("web_search", "calculator");
    private static final String MODEL = "gpt-4o";
    private static final String GOAL = "Research and answer the question";
    private static final String AGENT_ID = "test-agent";

    // ── plan-and-execute ──────────────────────────────────────────────────────

    @Test
    void planAndExecuteProducesCorrectStartNode() {
        var result = compile("plan-and-execute", 3);
        assertEquals("__plan__", result.get("start_node"));
    }

    @Test
    void planAndExecuteHasPlanNode() {
        var result = compile("plan-and-execute", 3);
        var nodes = nodes(result);
        assertTrue(nodes.containsKey("__plan__"), "Should have __plan__ node");
    }

    @Test
    void planAndExecuteHasCorrectNumberOfStepNodes() {
        var n = 4;
        var result = compile("plan-and-execute", n);
        var nodes = nodes(result);
        for (int i = 0; i < n; i++) {
            assertTrue(nodes.containsKey("__step_" + i + "__"),
                    "Should have __step_" + i + "__");
            assertTrue(nodes.containsKey("__cost_guard_" + i + "__"),
                    "Should have __cost_guard_" + i + "__");
        }
    }

    @Test
    void planAndExecuteHasFinalizeAndLimitExceeded() {
        var result = compile("plan-and-execute", 2);
        var nodes = nodes(result);
        assertTrue(nodes.containsKey("__finalize__"), "Should have __finalize__");
        assertTrue(nodes.containsKey("__limit_exceeded__"), "Should have __limit_exceeded__");
    }

    @Test
    void planAndExecuteEdgesGoToEnd() {
        var result = compile("plan-and-execute", 2);
        var edges = edges(result);
        assertTrue(edgesContain(edges, "__finalize__", "end"), "finalize → end edge should exist");
        assertTrue(edgesContain(edges, "__limit_exceeded__", "end"), "limit_exceeded → end edge should exist");
    }

    @Test
    void planAndExecuteHasStrategyMetadata() {
        var result = compile("plan-and-execute", 3);
        var meta = (Map<?, ?>) result.get("strategy_metadata");
        assertNotNull(meta);
        assertEquals("plan-and-execute", meta.get("strategy_name"));
        assertEquals(AGENT_ID, meta.get("agent_id"));
    }

    // ── react ─────────────────────────────────────────────────────────────────

    @Test
    void reactProducesCorrectStartNode() {
        var result = compile("react", 3);
        assertEquals("__think_0__", result.get("start_node"));
    }

    @Test
    void reactHasThinkNodesForEachIteration() {
        var n = 3;
        var result = compile("react", n);
        var nodes = nodes(result);
        for (int i = 0; i < n; i++) {
            assertTrue(nodes.containsKey("__think_" + i + "__"),
                    "Should have __think_" + i + "__");
            assertTrue(nodes.containsKey("__react_guard_" + i + "__"),
                    "Should have __react_guard_" + i + "__");
        }
    }

    @Test
    void reactHasObserveNodesForAllButLast() {
        var n = 3;
        var result = compile("react", n);
        var nodes = nodes(result);
        for (int i = 0; i < n - 1; i++) {
            assertTrue(nodes.containsKey("__observe_" + i + "__"),
                    "Should have __observe_" + i + "__");
        }
        assertFalse(nodes.containsKey("__observe_" + (n - 1) + "__"),
                "Should NOT have observe node for last iteration");
    }

    @Test
    void reactHasFinalizeAndLimitExceeded() {
        var result = compile("react", 2);
        var nodes = nodes(result);
        assertTrue(nodes.containsKey("__finalize__"));
        assertTrue(nodes.containsKey("__limit_exceeded__"));
    }

    @Test
    void reactNodeKindTypes() {
        var result = compile("react", 2);
        var nodes = nodes(result);

        var think0 = (Map<?, ?>) nodes.get("__think_0__");
        var kind = (Map<?, ?>) think0.get("kind");
        assertEquals("model", kind.get("type"));

        var guard = (Map<?, ?>) nodes.get("__react_guard_0__");
        var guardKind = (Map<?, ?>) guard.get("kind");
        assertEquals("condition", guardKind.get("type"));
    }

    // ── critic ────────────────────────────────────────────────────────────────

    @Test
    void criticProducesCorrectStartNode() {
        var result = compile("critic", 3);
        assertEquals("__draft__", result.get("start_node"));
    }

    @Test
    void criticHasDraftAndCriticNodes() {
        var result = compile("critic", 3);
        var nodes = nodes(result);
        assertTrue(nodes.containsKey("__draft__"));
        assertTrue(nodes.containsKey("__critic_0__"));
    }

    @Test
    void criticHasGatesAndReviseForNonFinalRounds() {
        // max_rounds defaults to min(3, maxIterations) = 3
        var result = compileWithConfig("critic", Map.of("max_rounds", 3), 5);
        var nodes = nodes(result);

        // Round 0 and 1 should have gates and revise nodes
        assertTrue(nodes.containsKey("__critic_gate_0__"), "Should have gate_0");
        assertTrue(nodes.containsKey("__revise_0__"), "Should have revise_0");
        assertTrue(nodes.containsKey("__critic_gate_1__"), "Should have gate_1");
        assertTrue(nodes.containsKey("__revise_1__"), "Should have revise_1");

        // Last round (2) should NOT have a gate (it goes directly to finalize)
        assertFalse(nodes.containsKey("__critic_gate_2__"), "Should NOT have gate_2");
        assertFalse(nodes.containsKey("__revise_2__"), "Should NOT have revise_2");
    }

    @Test
    void criticHasFinalizeAndLimitExceeded() {
        var result = compile("critic", 3);
        var nodes = nodes(result);
        assertTrue(nodes.containsKey("__finalize__"));
        assertTrue(nodes.containsKey("__limit_exceeded__"));
    }

    @Test
    void criticMaxRoundsCapAtMaxIterations() {
        // max_rounds = 10, but maxIterations = 2 → should be capped at 2
        var result = compileWithConfig("critic", Map.of("max_rounds", 10), 2);
        var nodes = nodes(result);

        // With 2 rounds: critic_0 (last round, goes direct to finalize), critic_1 (last)
        // Actually min(10,2) = 2, so rounds 0 and 1
        // round 0 is not last → has gate and revise; round 1 is last → no gate
        assertTrue(nodes.containsKey("__critic_0__"));
        assertTrue(nodes.containsKey("__critic_1__"));
        assertFalse(nodes.containsKey("__critic_2__"), "Should not have critic_2");
    }

    // ── Error handling ────────────────────────────────────────────────────────

    @Test
    void unknownStrategyThrows() {
        assertThrows(IllegalArgumentException.class, () ->
                StrategyCompiler.compile("unknown", Map.of(), TOOLS, MODEL, 3, 1.0, 60, GOAL, AGENT_ID));
    }

    @Test
    void invalidMaxIterationsThrows() {
        assertThrows(IllegalArgumentException.class, () ->
                StrategyCompiler.compile("react", Map.of(), TOOLS, MODEL, 0, 1.0, 60, GOAL, AGENT_ID));
    }

    @Test
    void invalidMaxCostThrows() {
        assertThrows(IllegalArgumentException.class, () ->
                StrategyCompiler.compile("react", Map.of(), TOOLS, MODEL, 3, 0.0, 60, GOAL, AGENT_ID));
    }

    @Test
    void invalidTimeoutThrows() {
        assertThrows(IllegalArgumentException.class, () ->
                StrategyCompiler.compile("react", Map.of(), TOOLS, MODEL, 3, 1.0, 0, GOAL, AGENT_ID));
    }

    // ── Node label checks ─────────────────────────────────────────────────────

    @Test
    void planNodeHasCorrectLabels() {
        var result = compile("plan-and-execute", 2);
        var nodes = nodes(result);
        var planNode = (Map<?, ?>) nodes.get("__plan__");
        var labels = (Map<?, ?>) planNode.get("labels");
        assertEquals("plan_generation", labels.get("jamjet.strategy.node"));
        assertEquals("plan_generated", labels.get("jamjet.strategy.event"));
    }

    @Test
    void limitExceededNodeHasLimitLabel() {
        var result = compile("react", 2);
        var nodes = nodes(result);
        var limitNode = (Map<?, ?>) nodes.get("__limit_exceeded__");
        var labels = (Map<?, ?>) limitNode.get("labels");
        assertEquals("true", labels.get("jamjet.strategy.limit"));
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    private Map<String, Object> compile(String strategy, int maxIterations) {
        return StrategyCompiler.compile(strategy, Map.of(), TOOLS, MODEL,
                maxIterations, 1.0, 60, GOAL, AGENT_ID);
    }

    private Map<String, Object> compileWithConfig(String strategy, Map<String, Object> config, int maxIterations) {
        return StrategyCompiler.compile(strategy, config, TOOLS, MODEL,
                maxIterations, 1.0, 60, GOAL, AGENT_ID);
    }

    @SuppressWarnings("unchecked")
    private Map<String, Object> nodes(Map<String, Object> result) {
        return (Map<String, Object>) result.get("nodes");
    }

    @SuppressWarnings("unchecked")
    private List<Map<String, Object>> edges(Map<String, Object> result) {
        return (List<Map<String, Object>>) result.get("edges");
    }

    private boolean edgesContain(List<Map<String, Object>> edges, String from, String to) {
        return edges.stream().anyMatch(e ->
                from.equals(e.get("from")) && to.equals(e.get("to")));
    }
}
