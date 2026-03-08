package dev.jamjet.eval;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * A dataset for evaluation, loaded from a JSONL file.
 *
 * <p>Each line is a JSON object with at minimum an {@code "input"} field.
 * Optional fields: {@code "expected_contains"} (list of strings), {@code "tags"} (list of strings).
 *
 * <p>Example JSONL row:
 * <pre>{@code
 * {"input": "What is 2+2?", "expected_contains": ["4"], "tags": ["math"]}
 * }</pre>
 */
public final class EvalDataset {

    private static final ObjectMapper MAPPER = new ObjectMapper();

    private final List<EvalRow> rows;

    private EvalDataset(List<EvalRow> rows) {
        this.rows = List.copyOf(rows);
    }

    /**
     * Load an {@link EvalDataset} from a JSONL file.
     *
     * @param path path to the {@code .jsonl} file
     * @return the loaded dataset
     * @throws IOException if the file cannot be read or parsed
     */
    public static EvalDataset fromFile(Path path) throws IOException {
        var lines = Files.readAllLines(path);
        var rows = new ArrayList<EvalRow>(lines.size());
        for (int i = 0; i < lines.size(); i++) {
            var line = lines.get(i).strip();
            if (line.isEmpty() || line.startsWith("//") || line.startsWith("#")) continue;
            try {
                var raw = MAPPER.readValue(line, new TypeReference<Map<String, Object>>() {});
                rows.add(EvalRow.fromMap(raw));
            } catch (IOException e) {
                throw new IOException("Failed to parse JSONL line " + (i + 1) + ": " + line, e);
            }
        }
        return new EvalDataset(rows);
    }

    /**
     * Create an in-memory dataset from a list of rows.
     */
    public static EvalDataset of(List<EvalRow> rows) {
        return new EvalDataset(rows);
    }

    /** All rows in this dataset. */
    public List<EvalRow> rows() {
        return rows;
    }

    public int size() {
        return rows.size();
    }

    // ── EvalRow ───────────────────────────────────────────────────────────────

    /**
     * A single row in an {@link EvalDataset}.
     */
    public record EvalRow(
            String input,
            List<String> expectedContains,
            List<String> tags,
            Map<String, Object> raw) {

        @SuppressWarnings("unchecked")
        static EvalRow fromMap(Map<String, Object> raw) {
            var input = (String) raw.getOrDefault("input", "");
            var expected = raw.get("expected_contains") instanceof List<?> list
                    ? (List<String>) list : List.<String>of();
            var tags = raw.get("tags") instanceof List<?> tagList
                    ? (List<String>) tagList : List.<String>of();
            return new EvalRow(input, expected, tags, raw);
        }
    }
}
