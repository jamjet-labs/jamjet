package dev.jamjet;

import dev.jamjet.tool.Tool;
import dev.jamjet.tool.ToolCall;
import dev.jamjet.tool.ToolDefinition;
import dev.jamjet.tool.ToolRegistry;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.*;

class ToolRegistryTest {

    // ── Tool fixtures ─────────────────────────────────────────────────────────

    @Tool(name = "web_search", description = "Search the web for information")
    record WebSearch(String query) implements ToolCall<String> {
        public String execute() { return "Results for: " + query; }
    }

    @Tool(description = "Get weather for a city")
    record GetWeather(String city, String unit) implements ToolCall<String> {
        public String execute() { return "Sunny in " + city; }
    }

    @Tool(description = "Tool with explicit name")
    record SomeComplexTool(String param) implements ToolCall<String> {
        public String execute() { return param; }
    }

    private ToolRegistry registry;

    @BeforeEach
    void setUp() {
        registry = new ToolRegistry();
    }

    @Test
    void registerAndRetrieveTool() {
        registry.register(WebSearch.class);

        var def = registry.get("web_search");
        assertNotNull(def, "Should find web_search");
        assertEquals("web_search", def.name());
        assertEquals("Search the web for information", def.description());
        assertEquals(WebSearch.class, def.cls());
    }

    @Test
    void toolNameDefaultsToSnakeCaseClassName() {
        registry.register(GetWeather.class);

        // "GetWeather" → "get_weather"
        var def = registry.get("get_weather");
        assertNotNull(def, "Should find get_weather");
        assertEquals("get_weather", def.name());
    }

    @Test
    void snakeCaseConversion() {
        assertEquals("web_search", ToolDefinition.toSnakeCase("WebSearch"));
        assertEquals("get_weather", ToolDefinition.toSnakeCase("GetWeather"));
        assertEquals("some_complex_tool", ToolDefinition.toSnakeCase("SomeComplexTool"));
        assertEquals("calculator", ToolDefinition.toSnakeCase("Calculator"));
        assertEquals("my_tool", ToolDefinition.toSnakeCase("MyTool"));
    }

    @Test
    void inputSchemaIsGeneratedForRecord() {
        registry.register(GetWeather.class);
        var def = registry.get("get_weather");
        assertNotNull(def.inputSchema(), "inputSchema should not be null");
        // The schema should be a map (JSON object)
        assertFalse(def.inputSchema().isEmpty(), "inputSchema should not be empty");
    }

    @Test
    void openAiFormatHasCorrectStructure() {
        registry.register(WebSearch.class);
        registry.register(GetWeather.class);

        var openAiTools = registry.toOpenAiFormat();
        assertEquals(2, openAiTools.size());

        for (var tool : openAiTools) {
            assertEquals("function", tool.get("type"), "type should be 'function'");
            var fn = (Map<?, ?>) tool.get("function");
            assertNotNull(fn, "function block should be present");
            assertNotNull(fn.get("name"), "function.name should be present");
            assertNotNull(fn.get("description"), "function.description should be present");
            assertNotNull(fn.get("parameters"), "function.parameters should be present");
        }
    }

    @Test
    void openAiFormatToolNames() {
        registry.register(WebSearch.class);
        var tools = registry.toOpenAiFormat();

        var names = tools.stream()
                .map(t -> ((Map<?, ?>) t.get("function")).get("name"))
                .toList();
        assertTrue(names.contains("web_search"), "Should contain web_search");
    }

    @Test
    void registryNamesReturnsAllRegistered() {
        registry.register(WebSearch.class);
        registry.register(GetWeather.class);

        var names = registry.names();
        assertEquals(2, names.size());
        assertTrue(names.contains("web_search"));
        assertTrue(names.contains("get_weather"));
    }

    @Test
    void registerThrowsForUnAnnotatedClass() {
        record NoAnnotation(String x) {}
        assertThrows(IllegalArgumentException.class, () -> registry.register(NoAnnotation.class));
    }

    @Test
    void getNullForUnregisteredTool() {
        assertNull(registry.get("not_registered"));
    }

    @Test
    void clearRemovesAllTools() {
        registry.register(WebSearch.class);
        assertNotNull(registry.get("web_search"));
        registry.clear();
        assertNull(registry.get("web_search"));
        assertTrue(registry.names().isEmpty());
    }

    @Test
    void fromClassDerivesDefinitionCorrectly() {
        var def = ToolDefinition.fromClass(WebSearch.class);
        assertEquals("web_search", def.name());
        assertEquals("Search the web for information", def.description());
        assertNotNull(def.inputSchema());
        assertEquals(WebSearch.class, def.cls());
    }
}
