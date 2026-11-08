package dev.jamjet.engram;

import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.io.InterruptedIOException;
import java.net.SocketTimeoutException;
import java.net.http.HttpTimeoutException;
import java.nio.channels.ClosedByInterruptException;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

class EngramConfigTokenTest {

    private final List<String> warnings = new ArrayList<>();

    private String resolve(String explicit, Map<String, String> env) {
        return EngramConfig.resolveToken(explicit, env::get, warnings::add);
    }

    @Test
    void theAuthTokenVariableIsReadWithoutAWarning() {
        assertEquals("a", resolve(null, Map.of("ENGRAM_AUTH_TOKEN", "a", "ENGRAM_TOKEN", "b")));
        assertTrue(warnings.isEmpty());
    }

    @Test
    void theOldVariableIsAFallbackWithAWarningThatHidesTheToken() {
        assertEquals("s3cret-legacy", resolve(null, Map.of("ENGRAM_TOKEN", "s3cret-legacy")));
        assertEquals(1, warnings.size());
        assertTrue(warnings.get(0).contains("ENGRAM_AUTH_TOKEN"));
        assertFalse(warnings.get(0).contains("s3cret-legacy"));
    }

    @Test
    void blankValuesCountAsUnset() {
        assertEquals("b", resolve(" ", Map.of("ENGRAM_AUTH_TOKEN", "", "ENGRAM_TOKEN", "b")));
        assertNull(resolve(null, Map.of()));
    }

    @Test
    void onlyARealInterruptCountsAsOne() {
        assertTrue(EngramClient.isInterrupt(new InterruptedIOException()));
        assertTrue(EngramClient.isInterrupt(new ClosedByInterruptException()));
        assertFalse(EngramClient.isInterrupt(new SocketTimeoutException()));
        assertFalse(EngramClient.isInterrupt(new HttpTimeoutException("timed out")));
        assertFalse(EngramClient.isInterrupt(new IOException("connection refused")));
    }
}
