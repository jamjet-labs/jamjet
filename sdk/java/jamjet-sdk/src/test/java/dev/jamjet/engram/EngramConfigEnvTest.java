package dev.jamjet.engram;

import org.junit.jupiter.api.Test;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * Surefire sets ENGRAM_AUTH_TOKEN (what the Engram server reads) and the old ENGRAM_TOKEN.
 * The server's variable must win.
 */
class EngramConfigEnvTest {

    @Test
    void theServersVariableWins() {
        assertEquals("auth-token-from-env", EngramConfig.defaults().apiToken());
    }

    @Test
    void anExplicitTokenWinsOverTheEnvironment() {
        assertEquals("explicit", EngramConfig.builder().apiToken("explicit").build().apiToken());
    }
}
