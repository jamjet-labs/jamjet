package dev.jamjet.ir;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Validates a {@link WorkflowIr} for structural and semantic correctness.
 *
 * <p>Mirrors the Rust runtime validator ({@code jamjet-ir/src/validate.rs}) to catch errors
 * client-side before submission. Validation rules:
 *
 * <ol>
 *   <li>workflow_id is non-empty</li>
 *   <li>version is valid semver (three numeric parts separated by dots)</li>
 *   <li>start_node is non-empty and exists in nodes</li>
 *   <li>all edge targets exist in nodes (or are {@code "end"})</li>
 *   <li>all nodes are reachable from start_node via BFS</li>
 *   <li>tool_ref, model_ref, mcp server, and remote agent references resolve</li>
 * </ol>
 *
 * <pre>{@code
 * var ir = workflow.compile();
 * var errors = IrValidator.validate(ir);
 * if (!errors.isEmpty()) {
 *     errors.forEach(System.err::println);
 * }
 * }</pre>
 */
public final class IrValidator {

    private IrValidator() {}

    /**
     * Validate the given IR and return a list of error messages.
     *
     * @param ir the workflow IR to validate
     * @return list of validation errors; empty if valid
     */
    public static List<String> validate(WorkflowIr ir) {
        var errors = new ArrayList<String>();
        validateMetadata(ir, errors);
        validateStartNode(ir, errors);
        validateEdges(ir, errors);
        validateReachability(ir, errors);
        validateRefs(ir, errors);
        return List.copyOf(errors);
    }

    /**
     * Validate and throw {@link IllegalStateException} if invalid.
     *
     * @param ir the workflow IR to validate
     * @throws IllegalStateException if any validation errors are found
     */
    public static void validateOrThrow(WorkflowIr ir) {
        var errors = validate(ir);
        if (!errors.isEmpty()) {
            throw new IllegalStateException(
                    "IR validation failed with " + errors.size() + " error(s):\n  - "
                            + String.join("\n  - ", errors));
        }
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    private static void validateMetadata(WorkflowIr ir, List<String> errors) {
        if (ir.id() == null || ir.id().isBlank()) {
            errors.add("workflow_id is empty");
        }

        var version = ir.version();
        if (version == null || version.isBlank()) {
            errors.add("version is empty");
            return;
        }
        var parts = version.split("\\.");
        if (parts.length != 3) {
            errors.add("Invalid version '" + version + "': must be semver (e.g. 0.1.0)");
            return;
        }
        for (var part : parts) {
            try {
                var n = Integer.parseUnsignedInt(part);
                if (n < 0) {
                    errors.add("Invalid version '" + version + "': parts must be non-negative integers");
                    return;
                }
            } catch (NumberFormatException e) {
                errors.add("Invalid version '" + version + "': '" + part + "' is not a valid integer");
                return;
            }
        }
    }

    // ── Start node ────────────────────────────────────────────────────────────

    private static void validateStartNode(WorkflowIr ir, List<String> errors) {
        if (ir.startNode() == null || ir.startNode().isBlank()) {
            errors.add("start_node is empty");
            return;
        }
        if (ir.nodes() == null || !ir.nodes().containsKey(ir.startNode())) {
            errors.add("start_node '" + ir.startNode() + "' does not exist in nodes");
        }
    }

    // ── Edges ─────────────────────────────────────────────────────────────────

    private static void validateEdges(WorkflowIr ir, List<String> errors) {
        if (ir.edges() == null || ir.nodes() == null) return;

        for (var edge : ir.edges()) {
            var to = edgeField(edge, "to");
            var from = edgeField(edge, "from");
            if (to == null) {
                errors.add("Edge from '" + from + "' has null target");
                continue;
            }
            if (!"end".equals(to) && !ir.nodes().containsKey(to)) {
                errors.add("Edge target '" + to + "' (from '" + from + "') does not exist in nodes");
            }
        }
    }

    // ── Reachability ──────────────────────────────────────────────────────────

    @SuppressWarnings("unchecked")
    private static void validateReachability(WorkflowIr ir, List<String> errors) {
        if (ir.nodes() == null || ir.edges() == null || ir.startNode() == null) return;
        if (!ir.nodes().containsKey(ir.startNode())) return; // already reported

        Set<String> visited = new HashSet<>();
        var queue = new ArrayDeque<String>();
        queue.add(ir.startNode());
        visited.add(ir.startNode());

        while (!queue.isEmpty()) {
            var current = queue.poll();

            // Follow top-level edges
            for (var edge : ir.edges()) {
                var from = edgeField(edge, "from");
                var to = edgeField(edge, "to");
                if (current.equals(from) && to != null && !"end".equals(to) && visited.add(to)) {
                    queue.add(to);
                }
            }

            // Follow condition branch targets embedded in node kind
            var nodeDef = ir.nodes().get(current);
            if (nodeDef instanceof Map<?, ?> nodeMap) {
                var kind = nodeMap.get("kind");
                if (kind instanceof Map<?, ?> kindMap && "condition".equals(kindMap.get("type"))) {
                    var branches = kindMap.get("branches");
                    if (branches instanceof List<?> branchList) {
                        for (var branch : branchList) {
                            if (branch instanceof Map<?, ?> branchMap) {
                                var target = branchMap.get("target");
                                if (target instanceof String t && !"end".equals(t) && visited.add(t)) {
                                    queue.add(t);
                                }
                            }
                        }
                    }
                }
            }
        }

        for (var nodeId : ir.nodes().keySet()) {
            if (!visited.contains(nodeId)) {
                // Skip limit_exceeded nodes — they are routed dynamically by the runtime
                var nodeDef = ir.nodes().get(nodeId);
                if (nodeDef instanceof Map<?, ?> nodeMap) {
                    var kind = nodeMap.get("kind");
                    if (kind instanceof Map<?, ?> kindMap && "limit_exceeded".equals(kindMap.get("type"))) {
                        continue;
                    }
                }
                errors.add("Node '" + nodeId + "' is unreachable from start_node '" + ir.startNode() + "'");
            }
        }
    }

    // ── Reference validation ──────────────────────────────────────────────────

    @SuppressWarnings("unchecked")
    private static void validateRefs(WorkflowIr ir, List<String> errors) {
        if (ir.nodes() == null) return;

        for (var entry : ir.nodes().entrySet()) {
            var nodeId = entry.getKey();
            var nodeDef = entry.getValue();
            if (!(nodeDef instanceof Map<?, ?> nodeMap)) continue;

            var kind = nodeMap.get("kind");
            if (!(kind instanceof Map<?, ?> kindMap)) continue;

            var type = String.valueOf(kindMap.get("type"));

            // Ref validation only applies when the corresponding definition map is non-empty.
            // Strategy-compiled agents use inline refs (e.g. model_ref = "gpt-4o") that are
            // resolved at runtime, not via the IR definition maps.
            switch (type) {
                case "tool" -> {
                    var toolRef = stringField(kindMap, "tool_ref");
                    if (toolRef != null && isNonEmpty(ir.tools()) && !ir.tools().containsKey(toolRef)) {
                        errors.add("Node '" + nodeId + "' references unknown tool '" + toolRef + "'");
                    }
                }
                case "model" -> {
                    var modelRef = stringField(kindMap, "model_ref");
                    if (modelRef != null && isNonEmpty(ir.models()) && !ir.models().containsKey(modelRef)) {
                        errors.add("Node '" + nodeId + "' references unknown model '" + modelRef + "'");
                    }
                }
                case "mcp_tool" -> {
                    var server = stringField(kindMap, "server");
                    if (server != null && isNonEmpty(ir.mcpServers()) && !ir.mcpServers().containsKey(server)) {
                        errors.add("Node '" + nodeId + "' references unknown MCP server '" + server + "'");
                    }
                }
                case "a2a_task" -> {
                    var remoteAgent = stringField(kindMap, "remote_agent");
                    if (remoteAgent != null && isNonEmpty(ir.remoteAgents()) && !ir.remoteAgents().containsKey(remoteAgent)) {
                        errors.add("Node '" + nodeId + "' references unknown remote agent '" + remoteAgent + "'");
                    }
                }
                default -> { /* no ref validation needed */ }
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    private static String edgeField(Map<String, Object> edge, String field) {
        var val = edge.get(field);
        return val != null ? val.toString() : null;
    }

    private static String stringField(Map<?, ?> map, String field) {
        var val = map.get(field);
        return val != null ? val.toString() : null;
    }

    private static boolean isNonEmpty(Map<?, ?> map) {
        return map != null && !map.isEmpty();
    }
}
