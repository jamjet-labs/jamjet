package dev.jamjet.engram;

import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

/**
 * A failed HTTP call must not leave the caller's thread interrupted unless it was
 * interrupted: a stray interrupt makes the caller's next blocking call fail.
 */
class EngramClientInterruptTest {

    private final EngramClient client = new EngramClient(
            EngramConfig.builder().baseUrl("http://127.0.0.1:1").apiToken("t").timeoutSeconds(2).build());

    @AfterEach
    void clearInterrupt() {
        Thread.interrupted();
    }

    @Test
    void aConnectionFailureDoesNotInterruptTheCaller() {
        assertThrows(RuntimeException.class, client::health);
        assertFalse(Thread.currentThread().isInterrupted(),
                "an IOException is not an interrupt; the flag must stay clear");
    }

    @Test
    void anInterruptIsKept() {
        Thread.currentThread().interrupt();
        assertThrows(RuntimeException.class, client::health);
        assertTrue(Thread.currentThread().isInterrupted(), "a real interrupt must be re-asserted");
    }
}
