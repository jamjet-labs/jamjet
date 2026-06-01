package dev.jamjet.loan.durable;

import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.runtime.instrument.DurabilityContext;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.LinkedHashMap;

/**
 * Persists and rehydrates {@link DurabilityContext} checkpoint state to disk.
 * Each run is stored as {@code <baseDir>/<runId>.json}: a JSON object whose
 * keys are checkpoint IDs in insertion order and whose values are the raw recorded
 * results.
 *
 * <p>Values are stored as their native JSON form (NOT {@code String.valueOf}), so a
 * recorded {@code int} round-trips back as a JSON integer and rehydrates as an
 * {@link Integer}. This is what lets a checkpoint survive a process restart: an
 * {@code int}-returning {@code @Checkpoint} method unboxes the replayed {@code Integer}
 * cleanly instead of hitting a {@code ClassCastException} on a {@code String}. Only
 * primitive/JSON-scalar checkpoints are persisted here (the loan pipeline records ints);
 * non-primitive results (e.g. the scoring {@code Decision}) are recomputed on resume and
 * never written.
 *
 * <p><b>Caveat:</b> rehydration is JSON-scalar fidelity only. A persisted JSON integer
 * comes back as {@link Integer}; a string comes back as {@link String}. Complex objects
 * are intentionally out of scope and must not be checkpointed through this store.
 *
 * <p>Writes are atomic: the payload is written to a temp file in the same
 * directory, then renamed with {@code ATOMIC_MOVE}, so a crash mid-write cannot
 * corrupt an existing checkpoint file.
 */
public class CheckpointStore {

    private static final ObjectMapper MAPPER = new ObjectMapper();
    private static final TypeReference<LinkedHashMap<String, Object>> MAP_TYPE =
            new TypeReference<>() {};
    private static final String CHECKPOINT_EXT = ".json";

    private final Path baseDir;

    public CheckpointStore(Path baseDir) {
        try {
            Files.createDirectories(baseDir);
        } catch (IOException e) {
            throw new UncheckedIOException("Cannot create checkpoint directory: " + baseDir, e);
        }
        this.baseDir = baseDir;
    }

    /**
     * Saves all recorded checkpoints from {@code ctx} under {@code runId}.
     * Existing file for the same runId is replaced atomically.
     *
     * @param runId stable run identifier; must be a plain name with no path separators (it becomes the file name).
     * @param ctx   the durability context whose recorded results will be persisted.
     */
    public void save(String runId, DurabilityContext ctx) {
        LinkedHashMap<String, Object> snapshot = new LinkedHashMap<>();
        for (String id : ctx.getCheckpointIds()) {
            // Store the raw recorded result so primitives round-trip with their JSON type
            // (an int comes back as an Integer, not the String "720"). The loan pipeline only
            // checkpoints JSON scalars (ints); complex results are recomputed on resume, never persisted.
            snapshot.put(id, ctx.getRecordedResult(id));
        }

        Path target = baseDir.resolve(runId + CHECKPOINT_EXT);
        try {
            Path tmp = Files.createTempFile(baseDir, ".cp-" + runId + "-", ".tmp");
            try {
                MAPPER.writeValue(tmp.toFile(), snapshot);
                Files.move(tmp, target,
                        StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
            } catch (IOException e) {
                // Best-effort cleanup of the temp file on failure.
                try { Files.deleteIfExists(tmp); } catch (IOException ignored) {}
                throw e;
            }
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to save checkpoint for run: " + runId, e);
        }
    }

    /**
     * Loads checkpoint state for {@code runId} and returns a {@link DurabilityContext}
     * pre-populated with all recorded values and set to replay mode.
     * Returns {@code null} if no checkpoint exists for this runId.
     */
    public DurabilityContext load(String runId) {
        Path target = baseDir.resolve(runId + CHECKPOINT_EXT);
        if (!Files.exists(target)) {
            return null;
        }
        try {
            LinkedHashMap<String, Object> snapshot = MAPPER.readValue(target.toFile(), MAP_TYPE);
            DurabilityContext ctx = DurabilityContext.create();
            for (var entry : snapshot.entrySet()) {
                ctx.recordResult(entry.getKey(), entry.getValue());
            }
            ctx.setReplayMode(true);
            return ctx;
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to load checkpoint for run: " + runId, e);
        }
    }
}
