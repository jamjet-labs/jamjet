package dev.jamjet.agent;

import java.lang.reflect.Proxy;

/**
 * Static factory for running agents via the {@link Task}-annotated interface pattern.
 *
 * <p>Creates a dynamic proxy for the given interface, reads the {@link Task} annotation,
 * constructs an {@link Agent}, runs it, and returns the string result.
 *
 * <pre>{@code
 * @Task(model = "gpt-4o", tools = { WebSearch.class }, strategy = "react")
 * interface ResearchTask {}
 *
 * String result = JamjetAgent.run(ResearchTask.class, "Summarize JamJet");
 * }</pre>
 */
public final class JamjetAgent {

    private JamjetAgent() {}

    /**
     * Run the agent described by the {@link Task}-annotated interface.
     *
     * <p>The interface must be annotated with {@link Task}. The {@code input} string is passed
     * directly to {@link Agent#run(String)}.
     *
     * @param taskInterface the {@link Task}-annotated interface class
     * @param input         the user prompt
     * @param <T>           the interface type
     * @return the agent's text output
     * @throws IllegalArgumentException if the interface is not annotated with {@link Task}
     */
    public static <T> String run(Class<T> taskInterface, String input) {
        var taskAnnotation = taskInterface.getAnnotation(Task.class);
        if (taskAnnotation == null) {
            throw new IllegalArgumentException(
                    taskInterface.getName() + " is not annotated with @Task");
        }

        // Build the agent from the annotation
        var agentBuilder = Agent.builder(taskInterface.getSimpleName())
                .model(taskAnnotation.model())
                .strategy(taskAnnotation.strategy())
                .maxIterations(taskAnnotation.maxIterations())
                .maxCostUsd(taskAnnotation.maxCostUsd())
                .timeoutSeconds(taskAnnotation.timeoutSeconds())
                .instructions(taskAnnotation.instructions())
                .tools(taskAnnotation.tools());

        var agent = agentBuilder.build();

        // Create a dynamic proxy (the interface is just a marker / descriptor)
        @SuppressWarnings("unchecked")
        var proxy = (T) Proxy.newProxyInstance(
                taskInterface.getClassLoader(),
                new Class<?>[]{taskInterface},
                (proxyInstance, method, args) -> {
                    // All method calls route through Agent.run
                    var prompt = (args != null && args.length > 0)
                            ? String.valueOf(args[0]) : input;
                    return agent.run(prompt).output();
                });

        // Run the agent directly
        var result = agent.run(input);
        return result.output();
    }

    /**
     * Create a dynamic proxy instance for the {@link Task}-annotated interface.
     *
     * <p>Each method call on the proxy is forwarded to the underlying agent. The first
     * string argument (if present) is used as the prompt; otherwise, the proxy was created
     * with a fixed {@code input}.
     *
     * @param taskInterface the {@link Task}-annotated interface class
     * @param <T>           the interface type
     * @return a proxy implementing the interface
     */
    public static <T> T proxy(Class<T> taskInterface) {
        var taskAnnotation = taskInterface.getAnnotation(Task.class);
        if (taskAnnotation == null) {
            throw new IllegalArgumentException(
                    taskInterface.getName() + " is not annotated with @Task");
        }

        var agent = Agent.builder(taskInterface.getSimpleName())
                .model(taskAnnotation.model())
                .strategy(taskAnnotation.strategy())
                .maxIterations(taskAnnotation.maxIterations())
                .maxCostUsd(taskAnnotation.maxCostUsd())
                .timeoutSeconds(taskAnnotation.timeoutSeconds())
                .instructions(taskAnnotation.instructions())
                .tools(taskAnnotation.tools())
                .build();

        @SuppressWarnings("unchecked")
        var proxy = (T) Proxy.newProxyInstance(
                taskInterface.getClassLoader(),
                new Class<?>[]{taskInterface},
                (proxyInstance, method, args) -> {
                    if (method.getDeclaringClass() == Object.class) {
                        return method.invoke(agent, args);
                    }
                    var prompt = (args != null && args.length > 0)
                            ? String.valueOf(args[0]) : "";
                    return agent.run(prompt).output();
                });

        return proxy;
    }
}
