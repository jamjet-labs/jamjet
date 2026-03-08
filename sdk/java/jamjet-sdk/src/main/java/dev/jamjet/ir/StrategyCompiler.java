package dev.jamjet.ir;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Compiles agent reasoning strategies (react, plan-and-execute, critic) into IR sub-DAGs.
 *
 * <p>The output is structurally identical to the Python SDK's {@code strategies.py} output.
 * Node naming convention: all strategy nodes are prefixed with {@code __} to avoid collisions
 * with user-defined nodes.
 *
 * <p>Compiled IR carries {@code strategy_metadata} in {@code labels} for observability.
 */
public final class StrategyCompiler {

    private StrategyCompiler() {}

    /**
     * Compile a named strategy into IR nodes + edges.
     *
     * @param strategyName   one of: {@code "react"}, {@code "plan-and-execute"}, {@code "critic"}
     * @param strategyConfig strategy-specific config (e.g., {@code "verifier_model"}, {@code "critic_model"})
     * @param tools          list of tool names available to the agent
     * @param model          LLM model reference (e.g., {@code "gpt-4o"})
     * @param maxIterations  maximum number of loop iterations
     * @param maxCostUsd     maximum cost in USD before halting
     * @param timeoutSeconds maximum wall-clock seconds before halting
     * @param goal           agent goal / instructions string
     * @param agentId        agent identifier
     * @return map with keys {@code nodes}, {@code edges}, {@code start_node}, {@code strategy_metadata}
     * @throws IllegalArgumentException if strategy name is unknown or limits are invalid
     */
    public static Map<String, Object> compile(
            String strategyName,
            Map<String, Object> strategyConfig,
            List<String> tools,
            String model,
            int maxIterations,
            double maxCostUsd,
            int timeoutSeconds,
            String goal,
            String agentId) {

        validateLimits(maxIterations, maxCostUsd, timeoutSeconds);

        Map<String, Object> result = switch (strategyName) {
            case "react" -> compilePlan(strategyConfig, tools, model, maxIterations, goal, agentId, "react");
            case "plan-and-execute" -> compilePlanAndExecute(strategyConfig, tools, model, maxIterations, goal, agentId);
            case "critic" -> compileCritic(strategyConfig, tools, model, maxIterations, goal, agentId);
            default -> throw new IllegalArgumentException(
                    "Unknown strategy '" + strategyName + "'. Known strategies: critic, plan-and-execute, react");
        };

        var metadata = new LinkedHashMap<String, Object>();
        metadata.put("strategy_name", strategyName);
        metadata.put("strategy_config", strategyConfig != null ? strategyConfig : Map.of());
        metadata.put("limits", Map.of(
                "max_iterations", maxIterations,
                "max_cost_usd", maxCostUsd,
                "timeout_seconds", timeoutSeconds));
        metadata.put("agent_id", agentId);
        result.put("strategy_metadata", metadata);

        return result;
    }

    private static void validateLimits(int maxIterations, double maxCostUsd, int timeoutSeconds) {
        if (maxIterations < 1) throw new IllegalArgumentException("maxIterations must be >= 1");
        if (maxCostUsd <= 0) throw new IllegalArgumentException("maxCostUsd must be > 0");
        if (timeoutSeconds < 1) throw new IllegalArgumentException("timeoutSeconds must be >= 1");
    }

    // ── plan-and-execute ──────────────────────────────────────────────────────

    private static Map<String, Object> compilePlanAndExecute(
            Map<String, Object> config,
            List<String> tools,
            String model,
            int n,
            String goal,
            String agentId) {

        var verifierModel = config != null ? (String) config.get("verifier_model") : null;
        var toolsList = tools != null && !tools.isEmpty() ? String.join(", ", tools) : "none";

        var nodes = new LinkedHashMap<String, Object>();
        var edges = new ArrayList<Map<String, Object>>();

        // Plan generation node
        nodes.put("__plan__", modelNode(
                model,
                "You are an AI agent working on: " + goal + "\n"
                        + "Available tools: " + toolsList + "\n"
                        + "Generate a structured plan with up to " + n + " concrete steps. "
                        + "Output JSON: {\"steps\": [\"step 1\", \"step 2\", ...]}",
                "__plan__",
                "You are a planning AI. Output valid JSON only.",
                Map.of("jamjet.strategy.node", "plan_generation",
                        "jamjet.strategy.event", "plan_generated")));

        edges.add(edge("__plan__", "__cost_guard_0__", null));

        for (int i = 0; i < n; i++) {
            var stepId = "__step_" + i + "__";
            var guardId = "__cost_guard_" + i + "__";
            var nextGuardId = "__cost_guard_" + (i + 1) + "__";

            // Cost guard node before this step
            nodes.put(guardId, conditionNode(
                    "state.__cost_exceeded__ == true",
                    costGuardBranches(stepId),
                    Map.of("jamjet.strategy.node", "cost_guard",
                            "jamjet.strategy.iteration", String.valueOf(i))));

            // Step executor node
            var stepPrompt = "You are executing step " + (i + 1) + " of " + n + " for the goal: " + goal + "\n"
                    + "The plan is: {{ state.__plan__ }}\n"
                    + "Execute step " + (i + 1) + " using the available tools. "
                    + "Record your result.";
            nodes.put(stepId, modelNode(
                    model, stepPrompt,
                    "__step_" + i + "_output__",
                    null,
                    Map.of("jamjet.strategy.node", "step_executor",
                            "jamjet.strategy.event", "iteration_started",
                            "jamjet.strategy.iteration", String.valueOf(i))));

            if (verifierModel != null) {
                var verifierId = "__verify_" + i + "__";
                nodes.put(verifierId, modelNode(
                        verifierModel,
                        "Verify step " + (i + 1) + " output against the goal: " + goal + "\n"
                                + "Output JSON: {\"passed\": true/false, \"score\": 0.0-1.0, \"feedback\": \"...\"}",
                        "__verify_" + i + "_result__",
                        null,
                        Map.of("jamjet.strategy.node", "verifier",
                                "jamjet.strategy.event", "critic_verdict")));
                edges.add(edge(stepId, verifierId, null));
                if (i < n - 1) {
                    edges.add(edge(verifierId, nextGuardId, null));
                } else {
                    edges.add(edge(verifierId, "__finalize__", null));
                }
            } else {
                if (i < n - 1) {
                    edges.add(edge(stepId, nextGuardId, null));
                } else {
                    edges.add(edge(stepId, "__finalize__", null));
                }
            }

            edges.add(edge(guardId, "__limit_exceeded__", "state.__cost_exceeded__ == true"));
            edges.add(edge(guardId, stepId, null));
        }

        // Finalizer
        nodes.put("__finalize__", modelNode(
                model,
                "You completed all steps for the goal: " + goal + "\n"
                        + "Synthesize the results from all steps into a final, well-structured response.",
                "result",
                null,
                Map.of("jamjet.strategy.node", "finalizer",
                        "jamjet.strategy.event", "strategy_completed")));
        edges.add(edge("__finalize__", "end", null));

        // Limit exceeded terminal
        nodes.put("__limit_exceeded__", limitExceededNode());
        edges.add(edge("__limit_exceeded__", "end", null));

        var result = new LinkedHashMap<String, Object>();
        result.put("nodes", nodes);
        result.put("edges", edges);
        result.put("start_node", "__plan__");
        return result;
    }

    // ── react ─────────────────────────────────────────────────────────────────

    private static Map<String, Object> compilePlan(
            Map<String, Object> config,
            List<String> tools,
            String model,
            int n,
            String goal,
            String agentId,
            String strategyHint) {
        // "react" strategy
        var toolsList = tools != null && !tools.isEmpty() ? String.join(", ", tools) : "none";

        var nodes = new LinkedHashMap<String, Object>();
        var edges = new ArrayList<Map<String, Object>>();

        for (int i = 0; i < n; i++) {
            var thinkId = "__think_" + i + "__";
            var observeId = "__observe_" + i + "__";
            var guardId = "__react_guard_" + i + "__";
            var nextThinkId = "__think_" + (i + 1) + "__";

            // Thought node
            nodes.put(thinkId, modelNode(
                    model,
                    "Goal: " + goal + "\n"
                            + "Available tools: " + toolsList + "\n"
                            + "Iteration " + (i + 1) + " of " + n + ". "
                            + "Think about what to do next. If you have enough information to answer, say FINISH. "
                            + "Otherwise choose a tool and describe what input to pass. "
                            + "Output JSON: {\"thought\": \"...\", \"action\": \"tool_name or FINISH\", \"input\": {...}}",
                    "__think_" + i + "_output__",
                    null,
                    Map.of("jamjet.strategy.node", "react_think",
                            "jamjet.strategy.event", "iteration_started",
                            "jamjet.strategy.iteration", String.valueOf(i))));

            // Cost guard
            var okTarget = (i < n - 1) ? observeId : "__finalize__";
            nodes.put(guardId, conditionNode(
                    "state.__cost_exceeded__ == true",
                    costGuardBranches(okTarget),
                    Map.of("jamjet.strategy.node", "cost_guard",
                            "jamjet.strategy.iteration", String.valueOf(i))));

            edges.add(edge(thinkId, guardId, null));
            edges.add(edge(guardId, "__limit_exceeded__", "state.__cost_exceeded__ == true"));
            edges.add(edge(guardId, okTarget, null));

            if (i < n - 1) {
                nodes.put(observeId, modelNode(
                        model,
                        "Goal: " + goal + "\n"
                                + "Previous thought: {{ state.__think_" + i + "_output__ }}\n"
                                + "Process the tool result and update your understanding. "
                                + "Output JSON: {\"observation\": \"...\", \"progress\": \"...\"}",
                        "__observe_" + i + "_output__",
                        null,
                        Map.of("jamjet.strategy.node", "react_observe",
                                "jamjet.strategy.event", "tool_called",
                                "jamjet.strategy.iteration", String.valueOf(i))));
                edges.add(edge(observeId, nextThinkId, null));
            }
        }

        // Finalizer
        nodes.put("__finalize__", modelNode(
                model,
                "Goal: " + goal + "\nBased on all thoughts and observations, produce a final answer.",
                "result",
                null,
                Map.of("jamjet.strategy.node", "finalizer",
                        "jamjet.strategy.event", "strategy_completed")));
        edges.add(edge("__finalize__", "end", null));

        nodes.put("__limit_exceeded__", limitExceededNode());
        edges.add(edge("__limit_exceeded__", "end", null));

        var result = new LinkedHashMap<String, Object>();
        result.put("nodes", nodes);
        result.put("edges", edges);
        result.put("start_node", "__think_0__");
        return result;
    }

    // ── critic ────────────────────────────────────────────────────────────────

    private static Map<String, Object> compileCritic(
            Map<String, Object> config,
            List<String> tools,
            String model,
            int maxIterations,
            String goal,
            String agentId) {

        var criticModel = config != null && config.get("critic_model") != null
                ? (String) config.get("critic_model") : model;
        var passThreshold = config != null && config.get("pass_threshold") != null
                ? ((Number) config.get("pass_threshold")).doubleValue() : 0.8;
        var configMaxRounds = config != null && config.get("max_rounds") != null
                ? ((Number) config.get("max_rounds")).intValue() : 3;
        var maxRounds = Math.min(configMaxRounds, maxIterations);

        var nodes = new LinkedHashMap<String, Object>();
        var edges = new ArrayList<Map<String, Object>>();

        // Initial draft
        nodes.put("__draft__", modelNode(
                model,
                "Goal: " + goal + "\nProduce a high-quality initial response.",
                "__draft_output__",
                null,
                Map.of("jamjet.strategy.node", "draft_generation",
                        "jamjet.strategy.event", "iteration_started")));
        edges.add(edge("__draft__", "__critic_0__", null));

        for (int i = 0; i < maxRounds; i++) {
            var criticId = "__critic_" + i + "__";
            var reviseId = "__revise_" + i + "__";
            var nextCriticId = "__critic_" + (i + 1) + "__";
            var draftRef = (i == 0) ? "__draft_output__" : "__revise_" + (i - 1) + "_output__";

            nodes.put(criticId, modelNode(
                    criticModel,
                    "Goal: " + goal + "\n"
                            + "Draft to evaluate: {{ state." + draftRef + " }}\n"
                            + "Evaluate this draft against the goal. Pass threshold: " + passThreshold + ".\n"
                            + "Output JSON: {\"score\": 0.0-1.0, \"passed\": true/false, \"feedback\": \"...\"}",
                    "__critic_" + i + "_verdict__",
                    null,
                    Map.of("jamjet.strategy.node", "critic_eval",
                            "jamjet.strategy.event", "critic_verdict",
                            "jamjet.strategy.iteration", String.valueOf(i))));

            if (i == maxRounds - 1) {
                edges.add(edge(criticId, "__finalize__", null));
            } else {
                var gateId = "__critic_gate_" + i + "__";
                var passedCond = "state.__critic_" + i + "_verdict__.passed == true";
                var gateBranch1 = new LinkedHashMap<String, Object>();
                gateBranch1.put("condition", passedCond);
                gateBranch1.put("target", "__finalize__");
                var gateBranch2 = new LinkedHashMap<String, Object>();
                gateBranch2.put("condition", null);
                gateBranch2.put("target", reviseId);
                nodes.put(gateId, conditionNode(
                        passedCond,
                        List.of(gateBranch1, gateBranch2),
                        Map.of("jamjet.strategy.node", "critic_gate",
                                "jamjet.strategy.iteration", String.valueOf(i))));
                edges.add(edge(criticId, gateId, null));
                edges.add(edge(gateId, "__finalize__", passedCond));
                edges.add(edge(gateId, reviseId, null));

                nodes.put(reviseId, modelNode(
                        model,
                        "Goal: " + goal + "\n"
                                + "Previous draft: {{ state." + draftRef + " }}\n"
                                + "Critic feedback: {{ state.__critic_" + i + "_verdict__.feedback }}\n"
                                + "Revise the draft based on the feedback.",
                        "__revise_" + i + "_output__",
                        null,
                        Map.of("jamjet.strategy.node", "revision",
                                "jamjet.strategy.event", "iteration_started",
                                "jamjet.strategy.iteration", String.valueOf(i + 1))));
                edges.add(edge(reviseId, nextCriticId, null));
            }
        }

        // Finalizer
        nodes.put("__finalize__", modelNode(
                model,
                "Goal: " + goal + "\nFormat the final, polished response based on all revisions.",
                "result",
                null,
                Map.of("jamjet.strategy.node", "finalizer",
                        "jamjet.strategy.event", "strategy_completed")));
        edges.add(edge("__finalize__", "end", null));

        nodes.put("__limit_exceeded__", limitExceededNode());
        edges.add(edge("__limit_exceeded__", "end", null));

        var result = new LinkedHashMap<String, Object>();
        result.put("nodes", nodes);
        result.put("edges", edges);
        result.put("start_node", "__draft__");
        return result;
    }

    // ── Node / edge helpers ───────────────────────────────────────────────────

    private static Map<String, Object> modelNode(
            String model,
            String prompt,
            String outputKey,
            String systemPrompt,
            Map<String, String> labels) {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "model");
        kind.put("model_ref", model);
        kind.put("prompt_ref", prompt);
        kind.put("output_schema", outputKey);
        kind.put("system_prompt", systemPrompt);

        var node = new LinkedHashMap<String, Object>();
        node.put("kind", kind);
        node.put("retry_policy", "llm_default");
        node.put("node_timeout_secs", null);
        node.put("description", prompt);
        node.put("labels", labels != null ? labels : Map.of());
        return node;
    }

    private static Map<String, Object> conditionNode(
            String expression,
            List<Map<String, Object>> branches,
            Map<String, String> labels) {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "condition");
        kind.put("branches", branches);
        kind.put("expression", expression);

        var node = new LinkedHashMap<String, Object>();
        node.put("kind", kind);
        node.put("retry_policy", null);
        node.put("node_timeout_secs", null);
        node.put("description", expression);
        node.put("labels", labels != null ? labels : Map.of());
        return node;
    }

    private static Map<String, Object> limitExceededNode() {
        var kind = new LinkedHashMap<String, Object>();
        kind.put("type", "limit_exceeded");
        kind.put("description", "Strategy limit reached");

        var node = new LinkedHashMap<String, Object>();
        node.put("kind", kind);
        node.put("retry_policy", null);
        node.put("node_timeout_secs", null);
        node.put("description", "Strategy limit exceeded — execution halted");
        node.put("labels", Map.of("jamjet.strategy.limit", "true"));
        return node;
    }

    private static Map<String, Object> edge(String from, String to, String condition) {
        var e = new LinkedHashMap<String, Object>();
        e.put("from", from);
        e.put("to", to);
        e.put("condition", condition);
        return e;
    }

    private static List<Map<String, Object>> costGuardBranches(String okTarget) {
        var b1 = new LinkedHashMap<String, Object>();
        b1.put("condition", "state.__cost_exceeded__");
        b1.put("target", "__limit_exceeded__");
        var b2 = new LinkedHashMap<String, Object>();
        b2.put("condition", null);
        b2.put("target", okTarget);
        return List.of(b1, b2);
    }
}
