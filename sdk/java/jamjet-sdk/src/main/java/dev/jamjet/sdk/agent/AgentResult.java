package dev.jamjet.sdk.agent;

import java.util.List;
import java.util.Map;

/**
 * Result returned by {@link Agent#run(String)}.
 *
 * @deprecated the result of the ungoverned in-process run; see {@link Agent#run(String)}.
 *     Not the same type as {@code dev.jamjet.agent.AgentResult} in {@code dev.jamjet:jamjet-agent}.
 */
@Deprecated(since = "0.6.0")
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
