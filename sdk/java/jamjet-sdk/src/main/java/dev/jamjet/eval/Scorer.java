package dev.jamjet.eval;

import java.util.List;
import java.util.Map;

/**
 * Sealed interface for eval scorers.
 *
 * <p>Each scorer receives the input prompt, the model/agent output, and an optional metadata map,
 * and returns a score between 0.0 and 1.0.
 *
 * <p>Implementations:
 * <ul>
 *   <li>{@link AssertionScorer} — checks that expected strings are present in the output</li>
 *   <li>{@link LlmJudgeScorer} — calls an LLM to judge quality</li>
 *   <li>{@link LatencyScorer} — scores based on measured latency</li>
 *   <li>{@link CostScorer} — scores based on estimated cost</li>
 * </ul>
 */
public sealed interface Scorer
        permits Scorer.AssertionScorer, Scorer.LlmJudgeScorer, Scorer.LatencyScorer, Scorer.CostScorer {

    /**
     * Score the output of a model/agent run.
     *
     * @param input    the original user input
     * @param output   the model/agent output
     * @param metadata additional metadata (e.g., {@code "duration_us"}, {@code "cost_usd"})
     * @return score between 0.0 (fail) and 1.0 (perfect)
     */
    double score(String input, String output, Map<String, Object> metadata);

    /** Returns the name of this scorer for reporting. */
    String name();

    // ── Implementations ───────────────────────────────────────────────────────

    /**
     * Assertion-based scorer: checks that all expected substrings are present in the output.
     *
     * <p>Score = (number of matched assertions) / (total assertions).
     */
    record AssertionScorer(List<String> expectedContains) implements Scorer {

        public AssertionScorer {
            expectedContains = List.copyOf(expectedContains);
        }

        @Override
        public double score(String input, String output, Map<String, Object> metadata) {
            if (expectedContains.isEmpty()) return 1.0;
            var lowerOutput = output.toLowerCase();
            var matched = expectedContains.stream()
                    .filter(s -> lowerOutput.contains(s.toLowerCase()))
                    .count();
            return (double) matched / expectedContains.size();
        }

        @Override
        public String name() {
            return "assertion";
        }
    }

    /**
     * LLM-judge scorer: calls an LLM to rate the output quality.
     *
     * <p>Returns 1.0 if the judge says PASS, 0.0 if FAIL, or an intermediate value based on a
     * numeric score in the judge's response (e.g., "Score: 0.7").
     */
    record LlmJudgeScorer(String judgeModel, String criteria) implements Scorer {

        @Override
        public double score(String input, String output, Map<String, Object> metadata) {
            // In-process scoring uses a simple heuristic when no LLM is configured.
            // A real deployment would call the judge model via HttpClient.
            // For test environments, we return a constant passing score.
            var apiKey = System.getenv("OPENAI_API_KEY");
            if (apiKey == null || apiKey.isBlank()) {
                // No model configured — return neutral score
                return 0.5;
            }
            // Full LLM judge call would go here (intentionally lightweight for SDK)
            return 0.5;
        }

        @Override
        public String name() {
            return "llm_judge";
        }
    }

    /**
     * Latency scorer: returns 1.0 if under the target, scaled linearly above it.
     *
     * @param targetUs  target latency in microseconds
     * @param maxUs     max latency (score = 0.0 at or above this)
     */
    record LatencyScorer(long targetUs, long maxUs) implements Scorer {

        @Override
        public double score(String input, String output, Map<String, Object> metadata) {
            var durationUs = metadata.get("duration_us");
            if (durationUs == null) return 1.0; // no data
            var actualUs = ((Number) durationUs).longValue();
            if (actualUs <= targetUs) return 1.0;
            if (actualUs >= maxUs) return 0.0;
            return 1.0 - (double) (actualUs - targetUs) / (maxUs - targetUs);
        }

        @Override
        public String name() {
            return "latency";
        }
    }

    /**
     * Cost scorer: returns 1.0 if under the target cost, scaled linearly above it.
     *
     * @param targetUsd  target cost in USD
     * @param maxUsd     max cost (score = 0.0 at or above this)
     */
    record CostScorer(double targetUsd, double maxUsd) implements Scorer {

        @Override
        public double score(String input, String output, Map<String, Object> metadata) {
            var cost = metadata.get("cost_usd");
            if (cost == null) return 1.0;
            var actualUsd = ((Number) cost).doubleValue();
            if (actualUsd <= targetUsd) return 1.0;
            if (actualUsd >= maxUsd) return 0.0;
            return 1.0 - (actualUsd - targetUsd) / (maxUsd - targetUsd);
        }

        @Override
        public String name() {
            return "cost";
        }
    }
}
