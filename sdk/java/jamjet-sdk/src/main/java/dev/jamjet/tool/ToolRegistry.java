package dev.jamjet.tool;

import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Central registry for JamJet tools in this JVM process.
 *
 * <p>Tools are registered by class (annotated with {@link Tool}). The registry is a singleton
 * accessible via {@link #global()}.
 *
 * <pre>{@code
 * ToolRegistry.global().register(WebSearch.class);
 * var tools = ToolRegistry.global().toOpenAiFormat();
 * }</pre>
 */
public final class ToolRegistry {

    private static final ToolRegistry GLOBAL = new ToolRegistry();

    private final Map<String, ToolDefinition> registry = new LinkedHashMap<>();

    public ToolRegistry() {}

    /** Return the global (process-wide) singleton registry. */
    public static ToolRegistry global() {
        return GLOBAL;
    }

    /**
     * Register a tool class. The class must be annotated with {@link Tool}.
     *
     * @param toolClass the tool class to register
     * @return this registry (for chaining)
     */
    public ToolRegistry register(Class<?> toolClass) {
        var def = ToolDefinition.fromClass(toolClass);
        registry.put(def.name(), def);
        return this;
    }

    /**
     * Look up a tool by name.
     *
     * @param name tool name
     * @return the definition, or {@code null} if not registered
     */
    public ToolDefinition get(String name) {
        return registry.get(name);
    }

    /** Return all registered tools as an unmodifiable map keyed by tool name. */
    public Map<String, ToolDefinition> all() {
        return Collections.unmodifiableMap(registry);
    }

    /**
     * Render all registered tools in the OpenAI function-calling format.
     *
     * <pre>{@code
     * [
     *   {
     *     "type": "function",
     *     "function": {
     *       "name": "web_search",
     *       "description": "Search the web",
     *       "parameters": { ... JSON Schema ... }
     *     }
     *   }
     * ]
     * }</pre>
     */
    public List<Map<String, Object>> toOpenAiFormat() {
        var result = new ArrayList<Map<String, Object>>(registry.size());
        for (var def : registry.values()) {
            result.add(Map.of(
                    "type", "function",
                    "function", Map.of(
                            "name", def.name(),
                            "description", def.description(),
                            "parameters", def.inputSchema()
                    )
            ));
        }
        return Collections.unmodifiableList(result);
    }

    /** Return the tool names registered in this registry. */
    public List<String> names() {
        return new ArrayList<>(registry.keySet());
    }

    /** Remove all registrations (useful in tests). */
    public void clear() {
        registry.clear();
    }
}
