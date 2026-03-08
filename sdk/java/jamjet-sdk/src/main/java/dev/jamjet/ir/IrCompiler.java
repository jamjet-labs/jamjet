package dev.jamjet.ir;

import dev.jamjet.agent.Agent;
import dev.jamjet.workflow.Workflow;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Compiles {@link Workflow} and {@link Agent} objects to canonical {@link WorkflowIr}.
 *
 * <p>This is the single compilation path — both the Workflow fluent API and the Agent builder
 * produce a {@link WorkflowIr} through this class.
 */
public final class IrCompiler {

    private IrCompiler() {}

    /**
     * Compile a {@link Workflow} to {@link WorkflowIr}.
     *
     * <p>Steps are compiled to {@code python_fn} nodes in declaration order. Edges follow
     * the step's routing configuration; default routing goes to the next step in sequence
     * (or {@code "end"} for the last step).
     */
    public static WorkflowIr compileWorkflow(Workflow workflow) {
        var steps = workflow.steps();
        if (steps.isEmpty()) {
            throw new IllegalStateException(
                    "Workflow '" + workflow.name() + "' has no steps defined");
        }

        var nodes = new LinkedHashMap<String, Object>();
        var edges = new ArrayList<Map<String, Object>>();

        for (int i = 0; i < steps.size(); i++) {
            var step = steps.get(i);
            var kind = new LinkedHashMap<String, Object>();
            kind.put("type", "python_fn");
            kind.put("module", "dev.jamjet.workflow");
            kind.put("function", step.name());
            kind.put("output_schema", "");

            var node = new LinkedHashMap<String, Object>();
            node.put("id", step.name());
            node.put("kind", kind);
            node.put("retry_policy", step.retryPolicy());
            node.put("node_timeout_secs", parseTimeout(step.timeout()));
            node.put("description", null);
            node.put("labels", Map.of());
            nodes.put(step.name(), node);

            // Routing
            var conditions = step.nextConditions();
            if (!conditions.isEmpty()) {
                for (var entry : conditions) {
                    edges.add(edgeMap(step.name(), entry.getKey(), null));
                }
            } else {
                var nextName = (i + 1 < steps.size()) ? steps.get(i + 1).name() : "end";
                edges.add(edgeMap(step.name(), nextName, null));
            }
        }

        var timeouts = new LinkedHashMap<String, Object>();
        timeouts.put("node_timeout", null);
        timeouts.put("workflow_timeout", null);
        timeouts.put("heartbeat_interval", 30);
        timeouts.put("approval_timeout", null);

        return new WorkflowIr(
                workflow.name(),
                workflow.version(),
                null,
                null,
                workflow.stateSchema(),
                steps.get(0).name(),
                nodes,
                edges,
                Map.of(),
                timeouts,
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of(),
                null);
    }

    /**
     * Compile an {@link Agent} to {@link WorkflowIr} using the strategy compiler.
     *
     * <p>Produces an agent-first workflow with all strategy nodes and limit guards wired
     * according to the agent's configuration.
     */
    public static WorkflowIr compileAgent(Agent agent) {
        var compiled = StrategyCompiler.compile(
                agent.strategy(),
                Map.of("goal_template", agent.instructions()),
                agent.toolNames(),
                agent.model(),
                agent.maxIterations(),
                agent.maxCostUsd(),
                agent.timeoutSeconds(),
                agent.instructions().isBlank() ? agent.name() : agent.instructions(),
                agent.name());

        @SuppressWarnings("unchecked")
        var rawNodes = (Map<String, Object>) compiled.get("nodes");
        @SuppressWarnings("unchecked")
        var rawEdges = (List<Map<String, Object>>) compiled.get("edges");
        var startNode = (String) compiled.get("start_node");
        @SuppressWarnings("unchecked")
        var strategyMeta = (Map<String, Object>) compiled.get("strategy_metadata");

        // Normalise nodes to IR format
        var nodes = new LinkedHashMap<String, Object>();
        for (var entry : rawNodes.entrySet()) {
            var nodeId = entry.getKey();
            @SuppressWarnings("unchecked")
            var nodeDef = (Map<String, Object>) entry.getValue();
            var normalised = new LinkedHashMap<String, Object>();
            normalised.put("id", nodeId);
            normalised.put("kind", nodeDef.get("kind"));
            normalised.put("retry_policy", nodeDef.get("retry_policy"));
            normalised.put("node_timeout_secs", nodeDef.get("node_timeout_secs"));
            normalised.put("description", nodeDef.get("description"));
            normalised.put("labels", nodeDef.getOrDefault("labels", Map.of()));
            nodes.put(nodeId, normalised);
        }

        return new WorkflowIr(
                agent.name(),
                "0.1.0",
                agent.name(),
                agent.instructions().isBlank() ? agent.name() : agent.instructions(),
                "",
                startNode,
                nodes,
                rawEdges,
                Map.of(),
                Map.of("workflow_timeout", agent.timeoutSeconds(), "heartbeat_interval", 30),
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of("jamjet.strategy", agent.strategy(), "jamjet.agent.id", agent.name()),
                strategyMeta);
    }

    private static Map<String, Object> edgeMap(String from, String to, String condition) {
        var e = new LinkedHashMap<String, Object>();
        e.put("from", from);
        e.put("to", to);
        e.put("condition", condition);
        return e;
    }

    private static Integer parseTimeout(String timeout) {
        if (timeout == null) return null;
        var s = timeout.strip();
        if (s.endsWith("s")) return Integer.parseInt(s.substring(0, s.length() - 1));
        if (s.endsWith("m")) return Integer.parseInt(s.substring(0, s.length() - 1)) * 60;
        if (s.endsWith("h")) return Integer.parseInt(s.substring(0, s.length() - 1)) * 3600;
        return Integer.parseInt(s);
    }
}
