package dev.jamjet.workflow;

import dev.jamjet.ir.IrCompiler;
import dev.jamjet.ir.WorkflowIr;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Function;

/**
 * Fluent workflow builder.
 *
 * <p>A workflow is a directed graph of named steps. Steps are executed sequentially by default,
 * or conditionally routed based on predicates. Use {@link #compile()} to get the canonical IR
 * for submission to the runtime, or {@link #run(Object)} for in-process execution.
 *
 * <pre>{@code
 * record MyState(String input, String output) {}
 *
 * var wf = Workflow.builder("my-workflow")
 *     .state(MyState.class)
 *     .step("fetch", state -> new MyState(state.input(), "fetched: " + state.input()))
 *     .step("process", state -> new MyState(state.input(), state.output().toUpperCase()))
 *     .build();
 *
 * var result = wf.run(new MyState("hello", null));
 * System.out.println(result.state().output()); // FETCHED: HELLO
 * }</pre>
 */
public final class Workflow {

    private static final Logger log = LoggerFactory.getLogger(Workflow.class);

    private final String name;
    private final String version;
    private final Class<?> stateClass;
    private final List<Step> steps;

    private Workflow(Builder<?> builder) {
        this.name = builder.name;
        this.version = builder.version;
        this.stateClass = builder.stateClass;
        this.steps = List.copyOf(builder.steps);
    }

    /** Start building a new workflow. */
    public static <S> Builder<S> builder(String name) {
        return new Builder<>(name);
    }

    public String name() {
        return name;
    }

    public String version() {
        return version;
    }

    public Class<?> stateClass() {
        return stateClass;
    }

    public List<Step> steps() {
        return steps;
    }

    /** The state schema string (fully-qualified class name). */
    public String stateSchema() {
        return stateClass != null ? stateClass.getName() : "";
    }

    /** Compile this workflow to the canonical {@link WorkflowIr}. */
    public WorkflowIr compile() {
        if (steps.isEmpty()) {
            throw new IllegalStateException("Workflow '" + name + "' has no steps");
        }
        return IrCompiler.compileWorkflow(this);
    }

    /**
     * Execute this workflow in-process.
     *
     * <p>Walks the step chain sequentially, applying each step's handler to the current state.
     * Conditional routing is evaluated after each step; if no condition matches the step's
     * {@code defaultNext} is used, or the next step in declaration order.
     *
     * @param initialState the starting state
     * @param <S>          state type
     * @return an {@link ExecutionResult} with the final state and execution metadata
     */
    @SuppressWarnings("unchecked")
    public <S> ExecutionResult<S> run(S initialState) {
        if (steps.isEmpty()) {
            throw new IllegalStateException("Workflow '" + name + "' has no steps");
        }

        var stateByName = new LinkedHashMap<String, Step>();
        for (var step : steps) {
            stateByName.put(step.name(), step);
        }

        var events = new ArrayList<Map<String, Object>>();
        var t0 = System.nanoTime();
        var state = initialState;
        var stepsExecuted = 0;
        var currentName = steps.get(0).name();
        var maxSteps = steps.size() * 10; // safety cap

        while (currentName != null && !currentName.equals("end") && stepsExecuted < maxSteps) {
            var step = stateByName.get(currentName);
            if (step == null) {
                log.warn("Step '{}' not found, ending workflow", currentName);
                break;
            }

            log.debug("Executing step '{}'", currentName);
            var tStep = System.nanoTime();
            state = (S) step.execute(state);
            stepsExecuted++;

            events.add(Map.of(
                    "step", currentName,
                    "duration_us", (System.nanoTime() - tStep) / 1000L));

            // Resolve next step
            var next = step.resolveNext(state);
            if (next == null) {
                // Fall back to next in declaration order
                var idx = steps.indexOf(step);
                next = (idx + 1 < steps.size()) ? steps.get(idx + 1).name() : "end";
            }
            currentName = next;
        }

        var totalUs = (System.nanoTime() - t0) / 1000L;
        return new ExecutionResult<>(state, stepsExecuted, totalUs, Collections.unmodifiableList(events));
    }

    // ── Builder ───────────────────────────────────────────────────────────────

    public static final class Builder<S> {

        private final String name;
        private String version = "0.1.0";
        private Class<?> stateClass;
        private final List<Step> steps = new ArrayList<>();

        private Builder(String name) {
            if (name == null || name.isBlank()) throw new IllegalArgumentException("Workflow name must not be blank");
            this.name = name;
        }

        public Builder<S> version(String version) {
            this.version = version;
            return this;
        }

        public Builder<S> state(Class<?> stateClass) {
            this.stateClass = stateClass;
            return this;
        }

        /** Add a simple step with just a handler function. */
        public Builder<S> step(String stepName, Function<S, S> handler) {
            steps.add(Step.builder(stepName).handler(handler).build());
            return this;
        }

        /** Add a fully-configured step. */
        public Builder<S> step(String stepName, Step step) {
            // If name differs from what's already in the step, wrap it
            if (step.name().equals(stepName)) {
                steps.add(step);
            } else {
                throw new IllegalArgumentException(
                        "Step name mismatch: expected '" + stepName + "' but step has name '" + step.name() + "'");
            }
            return this;
        }

        public Workflow build() {
            if (steps.isEmpty()) {
                throw new IllegalStateException("Workflow must have at least one step");
            }
            return new Workflow(this);
        }
    }
}
