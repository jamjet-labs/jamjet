package dev.jamjet.tool;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.github.victools.jsonschema.generator.OptionPreset;
import com.github.victools.jsonschema.generator.SchemaGenerator;
import com.github.victools.jsonschema.generator.SchemaGeneratorConfig;
import com.github.victools.jsonschema.generator.SchemaGeneratorConfigBuilder;
import com.github.victools.jsonschema.generator.SchemaVersion;

import java.util.Map;

/**
 * Immutable metadata for a registered JamJet tool.
 *
 * <p>Use {@link #fromClass(Class)} to derive a definition from a record class annotated
 * with {@link Tool}.
 */
public record ToolDefinition(
        String name,
        String description,
        Map<String, Object> inputSchema,
        Class<?> cls) {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    /**
     * Derive a {@link ToolDefinition} from a {@code @Tool}-annotated record class.
     *
     * <p>The class must be annotated with {@link Tool} and must be a Java record (or at
     * minimum have a usable JSON schema). The tool name defaults to the class name in
     * snake_case if {@link Tool#name()} is empty.
     *
     * @param cls the tool class
     * @return the derived definition
     * @throws IllegalArgumentException if the class is not annotated with {@link Tool}
     */
    public static ToolDefinition fromClass(Class<?> cls) {
        var ann = cls.getAnnotation(Tool.class);
        if (ann == null) {
            throw new IllegalArgumentException(
                    cls.getName() + " is not annotated with @Tool");
        }

        var toolName = ann.name().isBlank() ? toSnakeCase(cls.getSimpleName()) : ann.name();
        var description = ann.description();
        var inputSchema = generateSchema(cls);

        return new ToolDefinition(toolName, description, inputSchema, cls);
    }

    /** Convert a PascalCase or camelCase class name to snake_case. */
    public static String toSnakeCase(String name) {
        return name
                .replaceAll("([A-Z]+)([A-Z][a-z])", "$1_$2")
                .replaceAll("([a-z])([A-Z])", "$1_$2")
                .toLowerCase();
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Object> generateSchema(Class<?> cls) {
        SchemaGeneratorConfigBuilder configBuilder =
                new SchemaGeneratorConfigBuilder(SchemaVersion.DRAFT_2020_12, OptionPreset.PLAIN_JSON);
        SchemaGeneratorConfig config = configBuilder.build();
        SchemaGenerator generator = new SchemaGenerator(config);

        JsonNode schemaNode = generator.generateSchema(cls);
        return MAPPER.convertValue(schemaNode, Map.class);
    }
}
