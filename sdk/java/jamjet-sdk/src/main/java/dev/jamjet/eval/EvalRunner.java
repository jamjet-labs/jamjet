package dev.jamjet.eval;

import dev.jamjet.agent.Agent;
import dev.jamjet.workflow.Workflow;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.function.Function;

/**
 * Runs an {@link EvalDataset} against an agent or workflow, scoring each row with one or more
 * {@link Scorer}s.
 *
 * <pre>{@code
 * var results = EvalRunner.builder()
 *     .dataset(dataset)
 *     .agent(myAgent)
 *     .scorers(new Scorer.AssertionScorer(List.of("expected")))
 *     .parallelism(4)
 *     .failBelow(0.8)
 *     .build()
 *     .run();
 * }</pre>
 */
public final class EvalRunner {

    private static final Logger log = LoggerFactory.getLogger(EvalRunner.class);

    private final EvalDataset dataset;
    private final Function<String, String> runFn;
    private final List<Scorer> scorers;
    private final int parallelism;
    private final double failBelow;

    private EvalRunner(Builder builder) {
        this.dataset = builder.dataset;
        this.runFn = builder.runFn;
        this.scorers = List.copyOf(builder.scorers);
        this.parallelism = builder.parallelism;
        this.failBelow = builder.failBelow;
    }

    public static Builder builder() {
        return new Builder();
    }

    /**
     * Run the evaluation.
     *
     * @return list of {@link EvalResult} for each row
     * @throws EvalFailureException if the mean score falls below {@link Builder#failBelow(double)}
     */
    public List<EvalResult> run() {
        var rows = dataset.rows();
        var results = Collections.synchronizedList(new ArrayList<EvalResult>(rows.size()));

        var executor = Executors.newVirtualThreadPerTaskExecutor();
        var tasks = new ArrayList<Callable<EvalResult>>(rows.size());

        for (var row : rows) {
            tasks.add(() -> evalRow(row));
        }

        // Run with parallelism cap
        var batches = new ArrayList<List<Callable<EvalResult>>>();
        for (int i = 0; i < tasks.size(); i += parallelism) {
            batches.add(tasks.subList(i, Math.min(i + parallelism, tasks.size())));
        }

        try {
            for (var batch : batches) {
                var futures = new ArrayList<Future<EvalResult>>(batch.size());
                for (var task : batch) {
                    futures.add(executor.submit(task));
                }
                for (var future : futures) {
                    try {
                        results.add(future.get());
                    } catch (ExecutionException e) {
                        log.error("Eval row failed", e.getCause());
                        results.add(new EvalResult("", "ERROR: " + e.getCause().getMessage(),
                                Map.of(), false, 0L));
                    }
                }
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new RuntimeException("Eval interrupted", e);
        } finally {
            executor.shutdown();
        }

        // Check fail threshold
        if (failBelow > 0.0) {
            var mean = results.stream()
                    .mapToDouble(EvalResult::averageScore)
                    .average()
                    .orElse(1.0);
            if (mean < failBelow) {
                throw new EvalFailureException(mean, failBelow, results);
            }
        }

        return Collections.unmodifiableList(results);
    }

    private EvalResult evalRow(EvalDataset.EvalRow row) {
        var t0 = System.nanoTime();
        String output;
        try {
            output = runFn.apply(row.input());
        } catch (Exception e) {
            log.warn("Agent/workflow run failed for input '{}': {}", row.input(), e.getMessage());
            output = "ERROR: " + e.getMessage();
        }
        var durationUs = (System.nanoTime() - t0) / 1000L;

        var metadata = new LinkedHashMap<String, Object>();
        metadata.put("duration_us", durationUs);

        var scores = new LinkedHashMap<String, Double>();

        // Always include assertion scorer if the row has expected_contains
        if (!row.expectedContains().isEmpty()) {
            var assertionScorer = new Scorer.AssertionScorer(row.expectedContains());
            scores.put(assertionScorer.name(), assertionScorer.score(row.input(), output, metadata));
        }

        // Run configured scorers
        for (var scorer : scorers) {
            scores.put(scorer.name(), scorer.score(row.input(), output, metadata));
        }

        var passed = scores.values().stream().allMatch(s -> s >= failBelow);
        return new EvalResult(row.input(), output, Collections.unmodifiableMap(scores), passed, durationUs);
    }

    // ── Builder ───────────────────────────────────────────────────────────────

    public static final class Builder {

        private EvalDataset dataset;
        private Function<String, String> runFn;
        private final List<Scorer> scorers = new ArrayList<>();
        private int parallelism = 4;
        private double failBelow = 0.0;

        private Builder() {}

        public Builder dataset(EvalDataset dataset) {
            this.dataset = dataset;
            return this;
        }

        /** Run eval rows against an {@link Agent}. */
        public Builder agent(Agent agent) {
            this.runFn = prompt -> agent.run(prompt).output();
            return this;
        }

        /** Run eval rows against a {@link Workflow} with a state mapper. */
        public <S> Builder workflow(Workflow workflow, Function<String, S> inputMapper,
                Function<S, String> outputMapper) {
            this.runFn = input -> {
                var state = inputMapper.apply(input);
                @SuppressWarnings("unchecked")
                var result = workflow.run((S) state);
                return outputMapper.apply(result.state());
            };
            return this;
        }

        /** Provide a custom run function (input → output). */
        public Builder runFn(Function<String, String> runFn) {
            this.runFn = runFn;
            return this;
        }

        public Builder scorers(Scorer... scorers) {
            this.scorers.addAll(List.of(scorers));
            return this;
        }

        /** Maximum number of rows to run concurrently. Default: 4. */
        public Builder parallelism(int parallelism) {
            this.parallelism = parallelism;
            return this;
        }

        /**
         * Minimum average score required to pass. If the mean score falls below this, a
         * {@link EvalFailureException} is thrown after all rows complete. Default: 0.0 (disabled).
         */
        public Builder failBelow(double failBelow) {
            this.failBelow = failBelow;
            return this;
        }

        public EvalRunner build() {
            if (dataset == null) throw new IllegalStateException("dataset must be set");
            if (runFn == null) throw new IllegalStateException("agent or runFn must be set");
            return new EvalRunner(this);
        }
    }

    // ── EvalFailureException ──────────────────────────────────────────────────

    /** Thrown when the mean eval score falls below the configured threshold. */
    public static final class EvalFailureException extends RuntimeException {

        private final double actualScore;
        private final double threshold;
        private final List<EvalResult> results;

        public EvalFailureException(double actualScore, double threshold, List<EvalResult> results) {
            super("Eval failed: mean score %.3f < threshold %.3f".formatted(actualScore, threshold));
            this.actualScore = actualScore;
            this.threshold = threshold;
            this.results = List.copyOf(results);
        }

        public double actualScore() { return actualScore; }
        public double threshold() { return threshold; }
        public List<EvalResult> results() { return results; }
    }
}
