package dev.jamjet.workflow;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.function.Function;
import java.util.function.Predicate;

/**
 * A single step in a JamJet workflow.
 *
 * <p>Steps have a handler function, optional conditional routing, a timeout, and a retry policy.
 * Use {@link #builder(String)} for fluent construction.
 *
 * <pre>{@code
 * var step = Step.builder("process")
 *     .handler(state -> state)
 *     .timeout("30s")
 *     .build();
 * }</pre>
 */
public final class Step {

    private final String name;
    @SuppressWarnings("rawtypes")
    private final Function handler;
    private final List<Map.Entry<String, Predicate<Object>>> nextConditions;
    private final String defaultNext;
    private final String timeout;
    private final String retryPolicy;

    @SuppressWarnings("rawtypes")
    private Step(Builder builder) {
        this.name = builder.name;
        this.handler = builder.handler;
        this.nextConditions = List.copyOf(builder.nextConditions);
        this.defaultNext = builder.defaultNext;
        this.timeout = builder.timeout;
        this.retryPolicy = builder.retryPolicy;
    }

    public static Builder builder(String name) {
        return new Builder(name);
    }

    public String name() {
        return name;
    }

    @SuppressWarnings("unchecked")
    public <S> S execute(S state) {
        return (S) handler.apply(state);
    }

    /** Conditional routing entries: target step name → predicate. */
    public List<Map.Entry<String, Predicate<Object>>> nextConditions() {
        return nextConditions;
    }

    /** Default next step name when no condition matches. May be {@code null}. */
    public String defaultNext() {
        return defaultNext;
    }

    public String timeout() {
        return timeout;
    }

    public String retryPolicy() {
        return retryPolicy;
    }

    /** Determine the next step name given the current state, or {@code null} if at the end. */
    @SuppressWarnings("unchecked")
    public String resolveNext(Object state) {
        for (var entry : nextConditions) {
            if (entry.getValue().test(state)) {
                return entry.getKey();
            }
        }
        return defaultNext;
    }

    public static final class Builder {

        private final String name;
        @SuppressWarnings("rawtypes")
        private Function handler = Function.identity();
        private final List<Map.Entry<String, Predicate<Object>>> nextConditions = new ArrayList<>();
        private String defaultNext;
        private String timeout;
        private String retryPolicy;

        private Builder(String name) {
            if (name == null || name.isBlank()) throw new IllegalArgumentException("Step name must not be blank");
            this.name = name;
        }

        @SuppressWarnings({"unchecked", "rawtypes"})
        public <S> Builder handler(Function<S, S> handler) {
            this.handler = (Function) handler;
            return this;
        }

        /**
         * Add a conditional routing rule. Evaluated in declaration order; first match wins.
         *
         * @param targetStep name of the next step if the predicate returns {@code true}
         * @param condition  predicate evaluated against the current state
         */
        @SuppressWarnings("unchecked")
        public <S> Builder when(String targetStep, Predicate<S> condition) {
            nextConditions.add(Map.entry(targetStep, (Predicate<Object>) (Predicate<?>) condition));
            return this;
        }

        /** The default next step when no condition matches. */
        public Builder defaultNext(String defaultNext) {
            this.defaultNext = defaultNext;
            return this;
        }

        /** Timeout string (e.g., {@code "30s"}, {@code "5m"}). */
        public Builder timeout(String timeout) {
            this.timeout = timeout;
            return this;
        }

        /** Retry policy identifier. */
        public Builder retryPolicy(String retryPolicy) {
            this.retryPolicy = retryPolicy;
            return this;
        }

        public Step build() {
            return new Step(this);
        }
    }
}
