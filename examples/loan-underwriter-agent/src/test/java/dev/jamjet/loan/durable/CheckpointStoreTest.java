package dev.jamjet.loan.durable;

import dev.jamjet.runtime.instrument.DurabilityContext;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;
import static org.junit.jupiter.api.Assertions.*;

class CheckpointStoreTest {
    @Test
    void savesAndRehydratesInReplayMode(@TempDir Path dir) {
        var store = new CheckpointStore(dir);
        var ctx = DurabilityContext.create();
        DurabilityContext.setCurrent(ctx);
        try {
            ctx.replayOrExecute("credit", () -> "720");
            ctx.replayOrExecute("history", () -> "36");
            store.save("run-1", ctx);
        } finally {
            DurabilityContext.clear();
        }

        var resumed = store.load("run-1");
        assertNotNull(resumed);
        assertTrue(resumed.isReplayMode());
        // Supplier must NOT run on replay: sentinel proves the cached value is returned.
        String credit = resumed.replayOrExecute("credit", () -> { throw new AssertionError("re-ran"); });
        assertEquals("720", credit);
        assertEquals(java.util.List.of("credit", "history"), resumed.getCheckpointIds());
    }

    @Test
    void loadReturnsNullForUnknownRun(@TempDir Path dir) {
        assertNull(new CheckpointStore(dir).load("nope"));
    }

    @Test
    void roundtripsPrimitiveIntAcrossAFreshStore(@TempDir Path dir) {
        // Record an int checkpoint and persist it.
        var store = new CheckpointStore(dir);
        var ctx = DurabilityContext.create();
        DurabilityContext.setCurrent(ctx);
        try {
            int credit = ctx.replayOrExecute("credit", () -> 720);
            assertEquals(720, credit);
            store.save("run-int", ctx);
        } finally {
            DurabilityContext.clear();
        }

        // Reload via a FRESH store (simulates a process restart with empty memory).
        var resumed = new CheckpointStore(dir).load("run-int");
        assertNotNull(resumed);
        assertTrue(resumed.isReplayMode());

        // The rehydrated value must be an Integer (NOT the String "720"), so the agent's
        // int-returning checkpoint unboxes cleanly without ClassCastException.
        Object raw = resumed.getRecordedResult("credit");
        assertInstanceOf(Integer.class, raw, "checkpoint must rehydrate as Integer, not String");

        // Supplier must NOT run on replay; the cached Integer is returned and unboxes to int.
        assertEquals(720, (int) resumed.replayOrExecute("credit", () -> { throw new AssertionError("re-ran"); }));
    }
}
