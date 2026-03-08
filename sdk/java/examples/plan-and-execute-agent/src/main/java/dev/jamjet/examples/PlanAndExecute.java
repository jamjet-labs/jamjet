package dev.jamjet.examples;

import dev.jamjet.agent.Agent;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;

/**
 * Example: plan-and-execute strategy with multiple tools.
 *
 * <p>This strategy generates a structured plan first, then executes each step in sequence.
 * It's ideal for research and analysis tasks that benefit from up-front planning.
 *
 * <p>Run with:
 * <pre>
 *   export OPENAI_API_KEY=sk-...
 *   mvn exec:java -Dexec.mainClass=dev.jamjet.examples.PlanAndExecute
 * </pre>
 */
public class PlanAndExecute {

    // ── Tool definitions ──────────────────────────────────────────────────────

    @Tool(description = "Search the web for up-to-date information on a topic")
    record WebSearch(String query) implements ToolCall<String> {
        public String execute() {
            return "Web search results for '" + query + "': "
                    + "[Stub] JamJet is an open-source, performance-first agent runtime "
                    + "built in Rust. It supports MCP, A2A protocols, durable execution, "
                    + "and virtual-thread-based Java SDK.";
        }
    }

    @Tool(description = "Fetch and summarize the content of a URL")
    record FetchUrl(String url) implements ToolCall<String> {
        public String execute() {
            return "Content from '" + url + "': "
                    + "[Stub] This page describes the JamJet architecture, including "
                    + "the Rust runtime core, Python SDK, and Java SDK.";
        }
    }

    @Tool(description = "Store a note for later retrieval in this session")
    record StoreNote(String key, String content) implements ToolCall<String> {
        public String execute() {
            // In production this would write to a memory store
            System.out.println("  [Note stored] " + key + ": " + content.substring(0, Math.min(50, content.length())) + "...");
            return "Note stored successfully under key: " + key;
        }
    }

    // ── Main ──────────────────────────────────────────────────────────────────

    public static void main(String[] args) {
        var agent = Agent.builder("investment-researcher")
                .model("gpt-4o")
                .tools(WebSearch.class, FetchUrl.class, StoreNote.class)
                .instructions("""
                        You are a professional investment research analyst.
                        Your goal is to produce a structured investment memo about the given company.
                        - Search for recent news, financials, and competitive landscape.
                        - Fetch official sources when possible.
                        - Store key facts as notes for synthesis.
                        - Produce a final memo with: Executive Summary, Key Financials, Risks, Recommendation.
                        """)
                .strategy("plan-and-execute")
                .maxIterations(6)
                .maxCostUsd(0.50)
                .timeoutSeconds(120)
                .build();

        System.out.println("Plan-and-Execute Agent Example");
        System.out.println("==============================");
        System.out.println("Name:        " + agent.name());
        System.out.println("Model:       " + agent.model());
        System.out.println("Strategy:    " + agent.strategy());
        System.out.println("Tools:       " + agent.toolNames());
        System.out.println("MaxIter:     " + agent.maxIterations());
        System.out.println("MaxCost:     $" + agent.maxCostUsd());
        System.out.println();

        // Show compiled IR structure
        var ir = agent.compile();
        System.out.println("Compiled IR:");
        System.out.println("  workflow_id: " + ir.id());
        System.out.println("  start_node:  " + ir.startNode());
        System.out.println("  nodes:       " + ir.nodes().size());
        System.out.println("  edges:       " + ir.edges().size());

        var meta = ir.strategyMetadata();
        if (meta != null) {
            System.out.println("  strategy:    " + meta.get("strategy_name"));
        }
        System.out.println();

        var apiKey = System.getenv("OPENAI_API_KEY");
        if (apiKey == null || apiKey.isBlank()) {
            System.out.println("OPENAI_API_KEY not set — skipping live run.");
            System.out.println("The compiled IR above is ready for submission to the JamJet runtime.");
            return;
        }

        System.out.println("Running agent on 'JamJet, Inc.'...");
        System.out.println();
        var result = agent.run("Write an investment memo for JamJet, Inc., an AI agent runtime company.");

        System.out.println("Investment Memo:");
        System.out.println("----------------");
        System.out.println(result.output());
        System.out.println();
        System.out.printf("Duration:   %.2f ms%n", result.durationUs() / 1000.0);
        System.out.printf("Tool calls: %d%n", result.toolCalls().size());
    }
}
