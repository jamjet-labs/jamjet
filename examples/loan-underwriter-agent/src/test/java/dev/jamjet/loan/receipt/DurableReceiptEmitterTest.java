package dev.jamjet.loan.receipt;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

import java.nio.file.Path;

import static org.junit.jupiter.api.Assertions.*;

class DurableReceiptEmitterTest {

    @Test
    void emptyBundleWhenNoReceiptsOnDisk(@TempDir Path dir) {
        var emitter = new DurableReceiptEmitter(dir);
        var bundle = emitter.bundleFor("nobody");
        assertTrue(bundle.receipts().isEmpty());
        assertTrue(bundle.verify(), "an empty bundle vacuously verifies");
    }

    @Test
    void receiptsSurviveAFreshEmitterAndStillVerify(@TempDir Path dir) {
        var factory = new ReceiptFactory("loan-underwriter", "dev");

        // Process 1: emit two distinct receipts for one application to disk.
        var emitter1 = new DurableReceiptEmitter(dir);
        emitter1.emit(factory.forToolCall("app-d", "credit_bureau.score", "{\"id\":\"app-d\"}", "720"));
        emitter1.emit(factory.forToolCall("app-d", "account_history.months", "{\"id\":\"app-d\"}", "36"));

        // Process 2 (simulated restart): a brand-new emitter with empty memory reads from disk.
        var emitter2 = new DurableReceiptEmitter(dir);
        var bundle = emitter2.bundleFor("app-d");

        // MAKE-OR-BREAK: ActionReceipt survived the JSON disk roundtrip with its hash intact.
        assertEquals(2, bundle.receipts().size(), "both receipts must reload from disk");
        assertTrue(bundle.verify(), "reloaded bundle must verify (hashes recompute to stored values)");
    }

    @Test
    void reEmittingTheSameLogicalReceiptIsIdempotent(@TempDir Path dir) {
        var factory = new ReceiptFactory("loan-underwriter", "dev");

        var emitter = new DurableReceiptEmitter(dir);
        emitter.emit(factory.forToolCall("app-idem", "credit_bureau.score", "{\"id\":\"app-idem\"}", "720"));
        // Re-execute the SAME step (same app + tool). The deterministic receipt_id means this
        // overwrites the existing file rather than creating a second one.
        emitter.emit(factory.forToolCall("app-idem", "credit_bureau.score", "{\"id\":\"app-idem\"}", "720"));

        var reloaded = new DurableReceiptEmitter(dir);
        var bundle = reloaded.bundleFor("app-idem");
        assertEquals(1, bundle.receipts().size(), "re-emitting the same logical receipt must not duplicate");
        assertTrue(bundle.verify());
    }

    @Test
    void bundleIsOrderedDeterministically(@TempDir Path dir) {
        var factory = new ReceiptFactory("loan-underwriter", "dev");
        var emitter = new DurableReceiptEmitter(dir);
        // Emit in non-sorted order; bundleFor must return a stable, deterministic order
        // regardless of filesystem listing order.
        emitter.emit(factory.forToolCall("app-ord", "credit_bureau.score", "{}", "720"));
        emitter.emit(factory.forToolCall("app-ord", "account_history.months", "{}", "36"));
        emitter.emit(factory.forToolCall("app-ord", "underwriting.score", "{}", "APPROVE"));

        var a = new DurableReceiptEmitter(dir).bundleFor("app-ord").receipts();
        var b = new DurableReceiptEmitter(dir).bundleFor("app-ord").receipts();
        assertEquals(a.stream().map(r -> r.receiptId()).toList(),
                     b.stream().map(r -> r.receiptId()).toList(),
                     "ordering must be deterministic across reads");
    }
}
