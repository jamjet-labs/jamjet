package dev.jamjet.loan.receipt;

import com.fasterxml.jackson.databind.ObjectMapper;
import dev.jamjet.cloud.agentboundary.ActionReceipt;
import dev.jamjet.cloud.agentboundary.ActionReceiptEmitter;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.stream.Stream;

/**
 * Disk-backed {@link ActionReceiptEmitter}. Each receipt is written as a JSON file under
 * {@code <receiptsDir>/<applicationId>/<receiptId>.json}, so the audit trail survives the
 * process that produced it, including a {@code kill -9}.
 *
 * <p>This is what makes the post-crash audit bundle COMPLETE: an in-memory emitter loses
 * every pre-crash receipt when the process dies, and replayed steps do not re-emit (emission
 * happens inside the durable supplier, which is skipped on replay). Persisting each receipt
 * to disk means a fresh process can reconstruct the full bundle by re-reading the directory.
 *
 * <p>Writes are atomic (temp file + {@code ATOMIC_MOVE}) and keyed by {@code receiptId}, so a
 * re-emitted receipt (same logical step after a crash + resume, given {@link ReceiptFactory}'s
 * deterministic ids) overwrites its file rather than creating a duplicate. The grouping key is
 * read from {@code target.resource_id}, the same field {@link ReceiptFactory} writes the
 * {@code applicationId} into.
 */
public final class DurableReceiptEmitter implements ActionReceiptEmitter {

    private static final String EXT = ".json";

    /** Records carry {@code @JsonProperty} on every component, so a plain mapper roundtrips them. */
    private static final ObjectMapper MAPPER = new ObjectMapper().findAndRegisterModules();
    private final Path receiptsDir;

    public DurableReceiptEmitter(Path receiptsDir) {
        try {
            Files.createDirectories(receiptsDir);
        } catch (IOException e) {
            throw new UncheckedIOException("Cannot create receipts directory: " + receiptsDir, e);
        }
        this.receiptsDir = receiptsDir;
    }

    @Override
    public void emit(ActionReceipt receipt) {
        if (receipt == null || receipt.target() == null) {
            return;
        }
        String applicationId = receipt.target().resourceId();
        String receiptId = receipt.receiptId();
        if (applicationId == null || applicationId.isBlank() || receiptId == null || receiptId.isBlank()) {
            // Cannot address this receipt by application; skip persisting it (mirrors
            // CollectingReceiptEmitter, which also ignores ungrouped receipts).
            return;
        }

        Path appDir = receiptsDir.resolve(applicationId);
        try {
            Files.createDirectories(appDir);
            Path tgt = appDir.resolve(receiptId + EXT);
            Path tmp = Files.createTempFile(appDir, ".rc-" + receiptId + "-", ".tmp");
            try {
                MAPPER.writeValue(tmp.toFile(), receipt);
                // Keyed by receiptId so a re-emit overwrites (idempotent).
                Files.move(tmp, tgt,
                        StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING);
            } catch (IOException e) {
                try { Files.deleteIfExists(tmp); } catch (IOException ignored) {}
                throw e;
            }
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to persist receipt " + receiptId + " for " + applicationId, e);
        }
    }

    /**
     * Read every persisted receipt for {@code applicationId}, deserialize it, and return them
     * in a deterministic order (by {@code issued_at}, then {@code receipt_id}). Returns an empty
     * bundle if the application directory does not exist.
     */
    public AuditBundle bundleFor(String applicationId) {
        Path appDir = receiptsDir.resolve(applicationId);
        if (!Files.isDirectory(appDir)) {
            return new AuditBundle(List.of());
        }

        List<ActionReceipt> receipts = new ArrayList<>();
        try (Stream<Path> files = Files.list(appDir)) {
            for (Path p : files.filter(f -> f.getFileName().toString().endsWith(EXT)).toList()) {
                receipts.add(MAPPER.readValue(p.toFile(), ActionReceipt.class));
            }
        } catch (IOException e) {
            throw new UncheckedIOException("Failed to read receipts for application: " + applicationId, e);
        }

        // Best-effort order: by issuedAt, then receiptId. Same-millisecond receipts tie-break on id (lexicographic), not strict emission order. Fine for the one-call-per-tool pipeline.
        receipts.sort(Comparator
                .comparing(ActionReceipt::issuedAt)
                .thenComparing(ActionReceipt::receiptId));
        return new AuditBundle(receipts);
    }
}
