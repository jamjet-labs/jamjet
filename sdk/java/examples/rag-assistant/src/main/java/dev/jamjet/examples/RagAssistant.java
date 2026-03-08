package dev.jamjet.examples;

import dev.jamjet.agent.Agent;
import dev.jamjet.ir.WorkflowIr;
import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;
import dev.jamjet.workflow.ExecutionResult;
import dev.jamjet.workflow.Workflow;

import java.util.ArrayList;
import java.util.List;

/**
 * Example: RAG-style assistant using the Workflow builder.
 *
 * <p>This demonstrates a retrieval-augmented generation pattern built with JamJet's
 * {@link Workflow} API. The workflow has two steps:
 * <ol>
 *   <li><b>retrieve</b> — searches a document store for relevant context</li>
 *   <li><b>synthesize</b> — passes the retrieved context to a model to generate an answer</li>
 * </ol>
 *
 * <p>We also show an alternative: using an {@link Agent} with a retrieval tool.
 *
 * <p>Run with:
 * <pre>
 *   export OPENAI_API_KEY=sk-...
 *   mvn exec:java -Dexec.mainClass=dev.jamjet.examples.RagAssistant
 * </pre>
 */
public class RagAssistant {

    // ── State ─────────────────────────────────────────────────────────────────

    record RagState(
            String query,
            List<String> retrievedDocs,
            String answer) {}

    // ── Retrieval Tool (for agent-based RAG) ──────────────────────────────────

    @Tool(description = "Retrieve relevant documents from the knowledge base for a query")
    record RetrieveDocs(String query) implements ToolCall<String> {
        public String execute() {
            // Stub vector-search implementation
            return """
                    [Doc 1] JamJet is a performance-first agent runtime built in Rust. \
                    It provides durable graph-based workflow orchestration.

                    [Doc 2] JamJet supports MCP (Model Context Protocol) as the primary \
                    external tool protocol and A2A as the cross-framework agent protocol.

                    [Doc 3] The JamJet Java SDK uses virtual threads for non-blocking I/O \
                    and Jackson for JSON serialization.
                    """;
        }
    }

    @Tool(description = "Search the web for supplementary information")
    record WebSearch(String query) implements ToolCall<String> {
        public String execute() {
            return "Web: JamJet v0.1 released — open source, MIT license, "
                    + "Rust runtime, Python + Java SDKs.";
        }
    }

    // ── Workflow-based RAG ────────────────────────────────────────────────────

    /**
     * Build a RAG workflow using the {@link Workflow} builder.
     *
     * <p>In-process execution runs the two steps sequentially without needing
     * the JamJet runtime server.
     */
    static Workflow<RagState> buildRagWorkflow() {
        return Workflow.<RagState>builder("rag-assistant")
                .version("1.0.0")
                .state(RagState.class)
                // Step 1: Retrieve relevant documents from the knowledge base
                .step("retrieve", state -> {
                    var docs = retrieveDocuments(state.query());
                    return new RagState(state.query(), docs, null);
                })
                // Step 2: Synthesize an answer from the retrieved context
                .step("synthesize", state -> {
                    var context = String.join("\n\n", state.retrievedDocs());
                    var answer = synthesizeAnswer(state.query(), context);
                    return new RagState(state.query(), state.retrievedDocs(), answer);
                })
                .build();
    }

    private static List<String> retrieveDocuments(String query) {
        // Stub: in production, call a vector DB or search API
        System.out.println("  [Retrieve] Searching for: " + query);
        return List.of(
                "JamJet is a performance-first, agent-native runtime built in Rust.",
                "JamJet supports MCP and A2A protocols for tool use and agent communication.",
                "The Java SDK uses virtual threads and jackson for a modern, blocking API."
        );
    }

    private static String synthesizeAnswer(String query, String context) {
        // Stub: in production, call an LLM with the retrieved context
        System.out.println("  [Synthesize] Generating answer from " + context.split("\n").length + " context lines");
        return """
                Based on the retrieved documents:

                %s

                Answer to "%s":
                JamJet is a performance-first agent runtime built in Rust that provides \
                durable workflow orchestration, MCP/A2A protocol support, and modern SDKs \
                for Python and Java. The Java SDK leverages virtual threads for efficient \
                concurrent execution.
                """.formatted(context, query);
    }

    // ── Main ──────────────────────────────────────────────────────────────────

    public static void main(String[] args) {
        System.out.println("RAG Assistant Example");
        System.out.println("=====================");
        System.out.println();

        // ── Option 1: Workflow-based RAG ──────────────────────────────────────
        System.out.println("Option 1: Workflow-based RAG (in-process)");
        System.out.println("-----------------------------------------");

        var workflow = buildRagWorkflow();

        // Compile to IR
        WorkflowIr ir = workflow.compile();
        System.out.println("Compiled IR: " + ir.id() + " (" + ir.nodes().size() + " nodes)");
        System.out.println();

        // Run in-process
        var query = "How does JamJet handle concurrent tool calls in the Java SDK?";
        System.out.println("Query: " + query);
        ExecutionResult<RagState> result = workflow.run(new RagState(query, new ArrayList<>(), null));

        System.out.println();
        System.out.println("Answer:");
        System.out.println(result.state().answer());
        System.out.printf("Duration: %.2f ms (%d steps)%n",
                result.totalDurationUs() / 1000.0, result.stepsExecuted());

        System.out.println();
        System.out.println("Option 2: Agent-based RAG with retrieval tool");
        System.out.println("----------------------------------------------");

        var agent = Agent.builder("rag-agent")
                .model("gpt-4o-mini")
                .tools(RetrieveDocs.class, WebSearch.class)
                .instructions("""
                        You are a helpful assistant with access to a knowledge base.
                        Always retrieve relevant documents first, then synthesize a clear answer.
                        Cite the documents you used in your response.
                        """)
                .strategy("react")
                .maxIterations(4)
                .maxCostUsd(0.10)
                .timeoutSeconds(60)
                .build();

        System.out.println("Agent: " + agent.name() + ", tools: " + agent.toolNames());

        var apiKey = System.getenv("OPENAI_API_KEY");
        if (apiKey == null || apiKey.isBlank()) {
            System.out.println("OPENAI_API_KEY not set — showing IR only.");
            var agentIr = agent.compile();
            System.out.println("Compiled IR: " + agentIr.id()
                    + " (" + agentIr.nodes().size() + " nodes, start: " + agentIr.startNode() + ")");
        } else {
            System.out.println("Running agent...");
            var agentResult = agent.run(query);
            System.out.println("Answer: " + agentResult.output());
            System.out.printf("Duration: %.2f ms, tool calls: %d%n",
                    agentResult.durationUs() / 1000.0, agentResult.toolCalls().size());
        }
    }
}
