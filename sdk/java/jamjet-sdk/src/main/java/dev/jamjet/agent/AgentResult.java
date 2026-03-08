package dev.jamjet.agent;

import java.util.List;
import java.util.Map;

/**
 * Result returned by {@link Agent#run(String)}.
 */
public record AgentResult(
        String output,
        List<Map<String, Object>> toolCalls,
        Map<String, Object> ir,
        long durationUs) {

    /** Returns the agent's text output. Useful when implicitly converting to String. */
    @Override
    public String toString() {
        return output;
    }
}
