package dev.jamjet;

import dev.jamjet.sdk.agent.Agent;
import dev.jamjet.workflow.Workflow;
import dev.jamjet.workflow.WorkflowGraph;
import org.junit.jupiter.api.Test;

import java.util.Map;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * The runtime starts a workflow at version 1.0.0 when no version is given
 * ({@code runtime/api/src/routes.rs}, start_execution). A workflow registered without an
 * explicit version must be registered at that version, or starting it fails.
 */
class DefaultWorkflowVersionTest {

    record S(String v) {}

    @Test
    void workflowDefaultsTo100() {
        var wf = Workflow.<S>builder("wf").state(S.class).step("a", s -> s).build();
        assertEquals("1.0.0", wf.version());
        assertEquals("1.0.0", wf.compile().version());
    }

    @Test
    void graphDefaultsTo100() {
        var g = WorkflowGraph.builder("g").node("a", Map.of("type", "tool")).startNode("a").build();
        assertEquals("1.0.0", g.version());
    }

    @Test
    void compiledAgentDefaultsTo100() {
        var agent = Agent.builder("a").model("gpt-4o").build();
        assertEquals("1.0.0", agent.compile().version());
    }
}
