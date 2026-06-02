package dev.jamjet.loan;

import org.junit.jupiter.api.Test;
import org.springframework.boot.test.context.SpringBootTest;

@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.NONE)
class ContextLoadsTest {
    @Test
    void contextLoads() {
        // Passes only when the Spring context starts with all beans wired.
    }
}
