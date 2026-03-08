package dev.jamjet.agent;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Marks an interface as a JamJet agent task.
 *
 * <p>Used with {@link JamjetAgent#run(Class, String)} to create a dynamic-proxy-based agent
 * from an annotated interface.
 *
 * <pre>{@code
 * @Task(
 *     model = "gpt-4o",
 *     tools = { WebSearch.class },
 *     strategy = "react",
 *     maxIterations = 5
 * )
 * interface ResearchTask {
 *     String research(String topic);
 * }
 *
 * var result = JamjetAgent.run(ResearchTask.class, "What is JamJet?");
 * }</pre>
 */
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
public @interface Task {

    /** LLM model reference. */
    String model() default "gpt-4o";

    /** Tool classes to register for this agent. */
    Class<?>[] tools() default {};

    /** Reasoning strategy: {@code "plan-and-execute"}, {@code "react"}, or {@code "critic"}. */
    String strategy() default "plan-and-execute";

    /** Maximum number of loop iterations. */
    int maxIterations() default 10;

    /** Agent instructions / system prompt. */
    String instructions() default "";

    /** Maximum cost in USD before halting. */
    double maxCostUsd() default 1.0;

    /** Maximum wall-clock seconds before halting. */
    int timeoutSeconds() default 300;
}
