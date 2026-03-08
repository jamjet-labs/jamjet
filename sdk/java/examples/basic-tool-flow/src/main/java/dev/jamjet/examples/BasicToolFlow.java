package dev.jamjet.examples;

import dev.jamjet.agent.Agent;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;

/**
 * Basic example: a single agent with a web search tool using the react strategy.
 *
 * <p>Run with:
 * <pre>
 *   export OPENAI_API_KEY=sk-...
 *   mvn exec:java -Dexec.mainClass=dev.jamjet.examples.BasicToolFlow
 * </pre>
 */
public class BasicToolFlow {

    // ── Tool definition ───────────────────────────────────────────────────────

    @Tool(description = "Search the web for information about a topic")
    record WebSearch(String query) implements ToolCall<String> {
        public String execute() {
            // In a real implementation this would call a search API.
            // Here we return a stub response so the example runs without external deps.
            return "Results for '" + query + "': JamJet is a performance-first, agent-native "
                    + "runtime and framework for AI agents. It supports MCP and A2A protocols, "
                    + "durable graph-based workflow orchestration, and treats agents as "
                    + "first-class runtime entities.";
        }
    }

    // ── Main ──────────────────────────────────────────────────────────────────

    public static void main(String[] args) {
        var agent = Agent.builder("researcher")
                .model("claude-haiku-4-5-20251001")
                .tools(WebSearch.class)
                .instructions("You are a helpful research assistant. "
                        + "Always search first, then provide a thorough summary.")
                .strategy("react")
                .maxIterations(5)
                .build();

        System.out.println("Agent: " + agent.name());
        System.out.println("Model: " + agent.model());
        System.out.println("Strategy: " + agent.strategy());
        System.out.println("Tools: " + agent.toolNames());
        System.out.println();

        // Compile to IR and print structure
        var ir = agent.compile();
        System.out.println("Compiled IR:");
        System.out.println("  workflow_id: " + ir.id());
        System.out.println("  start_node:  " + ir.startNode());
        System.out.println("  nodes:       " + ir.nodes().size());
        System.out.println("  edges:       " + ir.edges().size());
        System.out.println();

        // Run the agent (requires OPENAI_API_KEY in environment)
        var apiKey = System.getenv("OPENAI_API_KEY");
        if (apiKey == null || apiKey.isBlank()) {
            System.out.println("OPENAI_API_KEY not set — skipping live run.");
            System.out.println("Set OPENAI_API_KEY to run the agent against the real API.");
            return;
        }

        System.out.println("Running agent...");
        var result = agent.run("What is JamJet?");
        System.out.println();
        System.out.println("Result:");
        System.out.println(result.output());
        System.out.println();
        System.out.printf("Duration: %.2f ms%n", result.durationUs() / 1000.0);
        System.out.printf("Tool calls: %d%n", result.toolCalls().size());
    }
}
