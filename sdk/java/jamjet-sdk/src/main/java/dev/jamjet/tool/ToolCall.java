package dev.jamjet.tool;

/**
 * Functional interface for a JamJet tool invocation.
 *
 * <p>Implement this interface on a record class annotated with {@link Tool}.
 * The record's components are the tool's input parameters; {@link #execute()}
 * performs the tool logic and returns the result.
 *
 * <pre>{@code
 * @Tool(description = "Retrieves weather for a city")
 * record GetWeather(String city, String unit) implements ToolCall<String> {
 *     public String execute() {
 *         return "Weather in " + city + ": sunny, 22" + unit;
 *     }
 * }
 * }</pre>
 *
 * @param <T> the return type of the tool invocation
 */
@FunctionalInterface
public interface ToolCall<T> {

    /**
     * Execute the tool and return the result.
     *
     * <p>This method is called by the JamJet runtime or in-process agent loop when the
     * model selects this tool. The record's constructor arguments are populated from the
     * model's JSON arguments before this method is called.
     *
     * @return the tool result (will be serialised to a string for the model)
     */
    T execute();
}
