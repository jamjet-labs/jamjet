package dev.jamjet.tool;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Marks a record class as a JamJet tool.
 *
 * <p>The annotated class must also implement {@link ToolCall}{@code <T>}.
 * Tool inputs are declared as record components; the JSON Schema is derived
 * automatically from the record's component types.
 *
 * <pre>{@code
 * @Tool(description = "Search the web for a query")
 * record WebSearch(String query) implements ToolCall<String> {
 *     public String execute() {
 *         return "Results for: " + query;
 *     }
 * }
 * }</pre>
 */
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
public @interface Tool {

    /**
     * The tool name exposed to the model.
     * Defaults to the class name converted to snake_case when empty.
     */
    String name() default "";

    /** Human-readable description of what this tool does. */
    String description();
}
