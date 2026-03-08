package dev.jamjet.workflow;

import dev.jamjet.ir.WorkflowIr;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Low-level graph-based workflow builder.
 *
 * <p>Use this when you need full control over node types and edges rather than the
 * step-based {@link Workflow} API. Each node is a raw {@code Map<String,Object>} matching
 * the IR node format.
 *
 * <pre>{@code
 * var graph = WorkflowGraph.builder("my-graph")
 *     .node("fetch", Map.of("type", "tool", "tool_ref", "web_search"))
 *     .node("summarize", Map.of("type", "model", "model_ref", "gpt-4o"))
 *     .edge("fetch", "summarize")
 *     .edge("summarize", "end")
 *     .startNode("fetch")
 *     .build();
 *
 * var ir = graph.compile();
 * }</pre>
 */
public final class WorkflowGraph {

    private final String name;
    private final String version;
    private final String startNode;
    private final Map<String, Map<String, Object>> nodes;
    private final List<Map<String, Object>> edges;

    private WorkflowGraph(Builder builder) {
        this.name = builder.name;
        this.version = builder.version;
        this.startNode = builder.startNode;
        this.nodes = Map.copyOf(builder.nodes);
        this.edges = List.copyOf(builder.edges);
    }

    /** Start building a new graph. */
    public static Builder builder(String name) {
        return new Builder(name);
    }

    public String name() {
        return name;
    }

    public String version() {
        return version;
    }

    public String startNode() {
        return startNode;
    }

    public Map<String, Map<String, Object>> nodes() {
        return nodes;
    }

    public List<Map<String, Object>> edges() {
        return edges;
    }

    /** Compile this graph to the canonical {@link WorkflowIr}. */
    public WorkflowIr compile() {
        if (startNode == null || startNode.isBlank()) {
            throw new IllegalStateException("WorkflowGraph '" + name + "' has no startNode set");
        }
        if (nodes.isEmpty()) {
            throw new IllegalStateException("WorkflowGraph '" + name + "' has no nodes");
        }

        // Wrap each raw config into the IR node format
        var irNodes = new LinkedHashMap<String, Object>();
        for (var entry : nodes.entrySet()) {
            var nodeId = entry.getKey();
            var config = entry.getValue();
            var node = new LinkedHashMap<String, Object>();
            node.put("id", nodeId);
            node.put("kind", config);
            node.put("retry_policy", null);
            node.put("node_timeout_secs", null);
            node.put("description", null);
            node.put("labels", Map.of());
            irNodes.put(nodeId, node);
        }

        return new WorkflowIr(
                name,
                version,
                null,
                null,
                "",
                startNode,
                irNodes,
                edges,
                Map.of(),
                timeoutsMap(),
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of(),
                Map.of(),
                null);
    }

    private static Map<String, Object> timeoutsMap() {
        var m = new LinkedHashMap<String, Object>();
        m.put("node_timeout", null);
        m.put("workflow_timeout", null);
        m.put("heartbeat_interval", 30);
        m.put("approval_timeout", null);
        return m;
    }

    // ── Builder ───────────────────────────────────────────────────────────────

    public static final class Builder {

        private final String name;
        private String version = "0.1.0";
        private String startNode;
        private final LinkedHashMap<String, Map<String, Object>> nodes = new LinkedHashMap<>();
        private final List<Map<String, Object>> edges = new ArrayList<>();

        private Builder(String name) {
            if (name == null || name.isBlank()) throw new IllegalArgumentException("Graph name must not be blank");
            this.name = name;
        }

        public Builder version(String version) {
            this.version = version;
            return this;
        }

        /**
         * Add a node to the graph.
         *
         * @param id     node identifier
         * @param config raw node config map (will become the {@code kind} field in the IR)
         */
        public Builder node(String id, Map<String, Object> config) {
            nodes.put(id, Map.copyOf(config));
            return this;
        }

        /** Add an unconditional edge from {@code from} to {@code to}. */
        public Builder edge(String from, String to) {
            var e = new LinkedHashMap<String, Object>();
            e.put("from", from);
            e.put("to", to);
            e.put("condition", null);
            edges.add(e);
            return this;
        }

        /** Add a conditional edge. */
        public Builder edge(String from, String to, String condition) {
            var e = new LinkedHashMap<String, Object>();
            e.put("from", from);
            e.put("to", to);
            e.put("condition", condition);
            edges.add(e);
            return this;
        }

        /** Set the start node ID. */
        public Builder startNode(String id) {
            this.startNode = id;
            return this;
        }

        public WorkflowGraph build() {
            return new WorkflowGraph(this);
        }
    }
}
