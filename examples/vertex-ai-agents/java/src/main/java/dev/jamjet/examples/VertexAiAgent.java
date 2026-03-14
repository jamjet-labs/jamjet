package dev.jamjet.examples;

import dev.jamjet.agent.Agent;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;

/**
 * JamJet + Vertex AI (Gemini) — Java Example
 *
 * <p>A research agent powered by Google Gemini, using JamJet's built-in
 * reasoning strategies and tool orchestration.
 *
 * <p>No JamJet CLI needed — runs as a standalone Java application.
 *
 * <h3>Setup</h3>
 * <pre>
 *   # Point JamJet at Gemini's OpenAI-compatible endpoint
 *   export OPENAI_BASE_URL="https://generativelanguage.googleapis.com/v1beta/openai/"
 *   export OPENAI_API_KEY="your-gemini-api-key"
 *
 *   # Build and run
 *   mvn compile exec:java -Dexec.mainClass=dev.jamjet.examples.VertexAiAgent
 * </pre>
 */
public class VertexAiAgent {

    // ── Tools ──────────────────────────────────────────────────────────────

    @Tool(description = "Search internal documents for relevant information about a topic")
    record SearchDocuments(String query) implements ToolCall<String> {
        public String execute() {
            // Stub — replace with your vector DB, Elasticsearch, or API call
            return switch (query.toLowerCase()) {
                case String q when q.contains("revenue") ->
                        "Q4 2025 revenue: $142M (+23% YoY). SaaS ARR: $98M. Gross margin: 74%.";
                case String q when q.contains("customer") ->
                        "Enterprise customers: 340 (+42 in Q4). Net retention: 127%. Avg deal: $288K.";
                case String q when q.contains("product") ->
                        "Launched agent orchestration v2 in Q3. 89 enterprise pilots. GA planned Q1 2026.";
                case String q when q.contains("competitor") ->
                        "Main competitors: LangChain (OSS), CrewAI ($18M Series A), Fixie.ai (acqui-hired).";
                default -> "No results for '" + query + "'.";
            };
        }
    }

    @Tool(description = "Get current stock price and key metrics for a ticker symbol")
    record GetStockData(String ticker) implements ToolCall<String> {
        public String execute() {
            // Stub — replace with a real market data API
            return switch (ticker.toUpperCase()) {
                case "GOOG" -> "GOOG: $182.45 | P/E: 24.3 | Market cap: $2.24T | YTD: +18.2%";
                case "MSFT" -> "MSFT: $448.20 | P/E: 35.1 | Market cap: $3.33T | YTD: +12.7%";
                case "AMZN" -> "AMZN: $213.80 | P/E: 42.6 | Market cap: $2.22T | YTD: +22.1%";
                default -> "No data for ticker '" + ticker + "'.";
            };
        }
    }

    @Tool(description = "Save a research note for later reference")
    record SaveNote(String title, String content) implements ToolCall<String> {
        public String execute() {
            System.out.println("  [Note] " + title + ": "
                    + content.substring(0, Math.min(80, content.length())) + "...");
            return "Note saved: " + title;
        }
    }

    // ── Main ───────────────────────────────────────────────────────────────

    public static void main(String[] args) {
        // Verify Gemini is configured
        var apiKey = System.getenv("OPENAI_API_KEY");
        var baseUrl = System.getenv("OPENAI_BASE_URL");

        if (apiKey == null || apiKey.isBlank()) {
            System.out.println("Error: Set OPENAI_API_KEY to your Gemini API key.");
            System.out.println();
            System.out.println("  export OPENAI_BASE_URL=\"https://generativelanguage.googleapis.com/v1beta/openai/\"");
            System.out.println("  export OPENAI_API_KEY=\"your-gemini-api-key\"");
            System.exit(1);
        }

        System.out.println("JamJet + Vertex AI (Gemini) — Java Example");
        System.out.println("==========================================");
        System.out.println("Endpoint: " + (baseUrl != null ? baseUrl : "(default)"));
        System.out.println();

        // ── Example 1: React agent (observe-reason-act loop) ───────────

        var researcher = Agent.builder("gemini_researcher")
                .model("gemini-2.0-flash")
                .tools(SearchDocuments.class, GetStockData.class, SaveNote.class)
                .instructions("""
                        You are an investment research analyst. \
                        Search for relevant data, check stock metrics, and save key findings. \
                        Produce a concise research summary with supporting data points.""")
                .strategy("react")
                .maxIterations(6)
                .maxCostUsd(0.50)
                .build();

        System.out.println("Agent:    " + researcher.name());
        System.out.println("Model:    " + researcher.model());
        System.out.println("Strategy: " + researcher.strategy());
        System.out.println("Tools:    " + researcher.toolNames());
        System.out.println();

        var prompt = args.length > 0
                ? String.join(" ", args)
                : "Research the AI agent platform market and summarize key players.";

        System.out.println("Prompt:   " + prompt);
        System.out.println("─".repeat(60));

        var result = researcher.run(prompt);

        System.out.println("─".repeat(60));
        System.out.println(result.output());
        System.out.println();
        System.out.printf("Tool calls: %d%n", result.toolCalls().size());
        System.out.printf("Duration:   %.2f ms%n", result.durationUs() / 1000.0);

        // ── Example 2: Plan-and-execute agent ──────────────────────────

        System.out.println();
        System.out.println("─".repeat(60));
        System.out.println("Example 2: Plan-and-Execute strategy");
        System.out.println("─".repeat(60));
        System.out.println();

        var analyst = Agent.builder("gemini_analyst")
                .model("gemini-2.0-flash")
                .tools(SearchDocuments.class, GetStockData.class)
                .instructions("""
                        You are a structured research analyst. \
                        Create a plan first, then execute each step methodically. \
                        Produce a final analysis with clear sections.""")
                .strategy("plan-and-execute")
                .maxIterations(5)
                .build();

        System.out.println("Agent:    " + analyst.name());
        System.out.println("Strategy: " + analyst.strategy());
        System.out.println();

        var result2 = analyst.run("Analyze revenue trends and competitive landscape in the AI agent market.");

        System.out.println(result2.output());
        System.out.println();
        System.out.printf("Tool calls: %d%n", result2.toolCalls().size());
        System.out.printf("Duration:   %.2f ms%n", result2.durationUs() / 1000.0);
    }
}
