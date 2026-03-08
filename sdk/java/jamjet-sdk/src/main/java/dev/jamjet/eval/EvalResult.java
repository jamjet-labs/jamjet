package dev.jamjet.eval;

import java.util.Map;

/**
 * Result for a single evaluation row.
 */
public record EvalResult(
        String input,
        String output,
        Map<String, Double> scores,
        boolean passed,
        long durationUs) {

    /**
     * Average of all scores in {@link #scores()}.
     */
    public double averageScore() {
        if (scores.isEmpty()) return 1.0;
        return scores.values().stream().mapToDouble(Double::doubleValue).average().orElse(0.0);
    }
}
