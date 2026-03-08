package dev.jamjet.workflow;

import java.util.List;
import java.util.Map;

/**
 * Result of an in-process workflow execution via {@link Workflow#run}.
 *
 * @param <S> the workflow's state type
 */
public record ExecutionResult<S>(
        S state,
        int stepsExecuted,
        long totalDurationUs,
        List<Map<String, Object>> events) {

    /** Convenience accessor for the final state. */
    public S finalState() {
        return state;
    }
}
