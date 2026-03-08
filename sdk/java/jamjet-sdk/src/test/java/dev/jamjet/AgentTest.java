package dev.jamjet;

import dev.jamjet.agent.Agent;
import dev.jamjet.agent.AgentResult;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;
import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class AgentTest {

    @Tool(description = "Search the web for information")
    record WebSearch(String query) implements ToolCall<String> {
        public String execute() {
            return "Results for: " + query;
        }
    }

    @Tool(description = "Calculate a math expression")
    record Calculator(String expression) implements ToolCall<String> {
        public String execute() {
            return "Result: " + expression;
        }
    }

    @Test
    void builderCreatesAgentWithDefaults() {
        var agent = Agent.builder("test-agent")
                .model("gpt-4o")
                .build();

        assertEquals("test-agent", agent.name());
        assertEquals("gpt-4o", agent.model());
        assertEquals("plan-and-execute", agent.strategy());
        assertEquals(10, agent.maxIterations());
        assertEquals(1.0, agent.maxCostUsd(), 1e-9);
        assertEquals(300, agent.timeoutSeconds());
        assertTrue(agent.instructions().isBlank());
        assertTrue(agent.toolNames().isEmpty());
    }

    @Test
    void builderRegistersTools() {
        var agent = Agent.builder("tool-agent")
                .model("gpt-4o")
                .tools(WebSearch.class, Calculator.class)
                .build();

        var toolNames = agent.toolNames();
        assertEquals(2, toolNames.size());
        assertTrue(toolNames.contains("web_search"), "Expected web_search in tools");
        assertTrue(toolNames.contains("calculator"), "Expected calculator in tools");
    }

    @Test
    void agentCompilesWithReactStrategy() {
        var agent = Agent.builder("react-agent")
                .model("gpt-4o-mini")
                .tools(WebSearch.class)
                .instructions("You are a research assistant.")
                .strategy("react")
                .maxIterations(3)
                .build();

        var ir = agent.compile();
        assertNotNull(ir);
        assertEquals("react-agent", ir.id());
        assertEquals("react", ir.labels().get("jamjet.strategy"));

        // React with maxIterations=3 produces: think_0..2, react_guard_0..2, observe_0..1, finalize, limit_exceeded
        var nodes = ir.nodes();
        assertTrue(nodes.containsKey("__think_0__"), "Should have think_0");
        assertTrue(nodes.containsKey("__finalize__"), "Should have finalize");
        assertTrue(nodes.containsKey("__limit_exceeded__"), "Should have limit_exceeded");
        assertEquals("__think_0__", ir.startNode());
    }

    @Test
    void agentCompilesWithPlanAndExecuteStrategy() {
        var agent = Agent.builder("plan-agent")
                .model("claude-haiku-4-5-20251001")
                .tools(WebSearch.class)
                .strategy("plan-and-execute")
                .maxIterations(2)
                .build();

        var ir = agent.compile();
        var nodes = ir.nodes();
        assertTrue(nodes.containsKey("__plan__"), "Should have plan node");
        assertTrue(nodes.containsKey("__step_0__"), "Should have step_0");
        assertTrue(nodes.containsKey("__step_1__"), "Should have step_1");
        assertTrue(nodes.containsKey("__finalize__"), "Should have finalize");
        assertEquals("__plan__", ir.startNode());
    }

    @Test
    void agentCompilesWithCriticStrategy() {
        var agent = Agent.builder("critic-agent")
                .model("gpt-4o")
                .strategy("critic")
                .maxIterations(3)
                .build();

        var ir = agent.compile();
        var nodes = ir.nodes();
        assertTrue(nodes.containsKey("__draft__"), "Should have draft node");
        assertTrue(nodes.containsKey("__critic_0__"), "Should have critic_0");
        assertTrue(nodes.containsKey("__finalize__"), "Should have finalize");
        assertEquals("__draft__", ir.startNode());
    }

    @Test
    void agentResultToString() {
        var result = new AgentResult(
                "This is the output",
                List.of(Map.of("tool", "web_search", "input", Map.of("query", "test"))),
                Map.of("strategy", "react"),
                12345L);

        assertEquals("This is the output", result.toString());
        assertEquals("This is the output", result.output());
        assertEquals(1, result.toolCalls().size());
        assertEquals(12345L, result.durationUs());
    }

    @Test
    void agentCompilesStrategyMetadata() {
        var agent = Agent.builder("meta-agent")
                .model("gpt-4o")
                .strategy("plan-and-execute")
                .maxIterations(5)
                .maxCostUsd(2.0)
                .timeoutSeconds(120)
                .build();

        var ir = agent.compile();
        var meta = ir.strategyMetadata();
        assertNotNull(meta, "strategy_metadata should be present");
        assertEquals("plan-and-execute", meta.get("strategy_name"));
        assertEquals("meta-agent", meta.get("agent_id"));

        @SuppressWarnings("unchecked")
        var limits = (java.util.Map<String, Object>) meta.get("limits");
        assertEquals(5, limits.get("max_iterations"));
        assertEquals(2.0, ((Number) limits.get("max_cost_usd")).doubleValue(), 1e-9);
        assertEquals(120, limits.get("timeout_seconds"));
    }

    @Test
    void unknownStrategyThrows() {
        var agent = Agent.builder("bad-agent")
                .model("gpt-4o")
                .strategy("unknown-strategy")
                .build();

        assertThrows(IllegalArgumentException.class, agent::compile);
    }
}
