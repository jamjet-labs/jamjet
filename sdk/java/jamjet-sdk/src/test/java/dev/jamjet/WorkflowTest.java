package dev.jamjet;

import dev.jamjet.ir.WorkflowIr;
import dev.jamjet.workflow.ExecutionResult;
import dev.jamjet.workflow.Step;
import dev.jamjet.workflow.Workflow;
import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class WorkflowTest {

    record TestState(String input, String output, boolean flag) {}

    @Test
    void builderCompilesLinearWorkflow() {
        var wf = Workflow.<TestState>builder("test-workflow")
                .version("1.0.0")
                .state(TestState.class)
                .step("step1", s -> new TestState(s.input(), "step1:" + s.input(), s.flag()))
                .step("step2", s -> new TestState(s.input(), s.output() + "|step2", s.flag()))
                .build();

        assertEquals("test-workflow", wf.name());
        assertEquals("1.0.0", wf.version());
        assertEquals(2, wf.steps().size());

        var ir = wf.compile();
        assertNotNull(ir);
        assertEquals("test-workflow", ir.id());
        assertEquals("1.0.0", ir.version());
        assertEquals("step1", ir.startNode());
        assertEquals(2, ir.nodes().size());
        assertTrue(ir.nodes().containsKey("step1"));
        assertTrue(ir.nodes().containsKey("step2"));
        assertEquals(2, ir.edges().size());
    }

    @Test
    void inProcessExecutionRunsAllSteps() {
        var wf = Workflow.<TestState>builder("exec-test")
                .state(TestState.class)
                .step("fetch", s -> new TestState(s.input(), "fetched:" + s.input(), s.flag()))
                .step("process", s -> new TestState(s.input(), s.output().toUpperCase(), s.flag()))
                .build();

        var result = wf.run(new TestState("hello", null, false));

        assertNotNull(result);
        assertEquals(2, result.stepsExecuted());
        assertEquals("FETCHED:HELLO", result.state().output());
        assertTrue(result.totalDurationUs() >= 0);
        assertEquals(2, result.events().size());
    }

    @Test
    void conditionalRoutingTakesCorrectBranch() {
        var stepA = Step.<TestState>builder("start")
                .handler((TestState s) -> new TestState(s.input(), "from_start", s.flag()))
                .when("branch_true", TestState::flag)
                .defaultNext("branch_false")
                .build();

        var wf = Workflow.<TestState>builder("routing-test")
                .state(TestState.class)
                .step("start", stepA)
                .step("branch_true", s -> new TestState(s.input(), s.output() + "|true", s.flag()))
                .step("branch_false", s -> new TestState(s.input(), s.output() + "|false", s.flag()))
                .build();

        // flag=true → should go to branch_true
        var resultTrue = wf.run(new TestState("hi", null, true));
        assertTrue(resultTrue.state().output().contains("|true"), "Expected true branch");

        // flag=false → should go to branch_false
        var resultFalse = wf.run(new TestState("hi", null, false));
        assertTrue(resultFalse.state().output().contains("|false"), "Expected false branch");
    }

    @Test
    void irRoundTripPreservesAllFields() {
        var wf = Workflow.<TestState>builder("rt-workflow")
                .state(TestState.class)
                .step("only_step", s -> s)
                .build();

        var ir = wf.compile();
        var json = ir.toJson();
        var restored = WorkflowIr.fromJson(json);

        assertEquals(ir.id(), restored.id());
        assertEquals(ir.version(), restored.version());
        assertEquals(ir.startNode(), restored.startNode());
        assertNotNull(restored.nodes());
        assertNotNull(restored.edges());
    }

    @Test
    void emptyWorkflowThrowsOnRun() {
        assertThrows(IllegalStateException.class, () -> {
            // Builder requires at least one step, so we catch at build time
            Workflow.<TestState>builder("empty")
                    .state(TestState.class)
                    .build();
        });
    }

    @Test
    void irNodesContainExpectedKindType() {
        var wf = Workflow.<TestState>builder("kind-check")
                .state(TestState.class)
                .step("myStep", s -> s)
                .build();

        var ir = wf.compile();
        @SuppressWarnings("unchecked")
        var node = (Map<String, Object>) ir.nodes().get("myStep");
        assertNotNull(node, "Node 'myStep' should exist");
        @SuppressWarnings("unchecked")
        var kind = (Map<String, Object>) node.get("kind");
        assertNotNull(kind);
        assertEquals("python_fn", kind.get("type"));
    }

    @Test
    void executionResultEvents() {
        var wf = Workflow.<TestState>builder("events-test")
                .state(TestState.class)
                .step("a", s -> new TestState(s.input(), "a", s.flag()))
                .step("b", s -> new TestState(s.input(), "b", s.flag()))
                .step("c", s -> new TestState(s.input(), "c", s.flag()))
                .build();

        var result = wf.run(new TestState("x", null, false));
        assertEquals(3, result.stepsExecuted());
        assertEquals(3, result.events().size());

        var event = result.events().get(0);
        assertEquals("a", event.get("step"));
        assertTrue(event.containsKey("duration_us"));
    }
}
