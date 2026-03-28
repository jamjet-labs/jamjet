package dev.jamjet.skillsjars;

import org.junit.jupiter.api.Test;

import java.io.IOException;
import java.nio.charset.StandardCharsets;

import static org.junit.jupiter.api.Assertions.*;

class SkillManifestTest {

    @Test
    void skillManifestIsLoadableAndHasRequiredFields() throws IOException {
        try (var stream = getClass().getClassLoader()
                .getResourceAsStream("META-INF/skills/SKILL.md")) {
            assertNotNull(stream, "SKILL.md must be on the classpath");

            String content = new String(stream.readAllBytes(), StandardCharsets.UTF_8);

            assertTrue(content.contains("name:"), "SKILL.md must have a 'name' field");
            assertTrue(content.contains("description:"), "SKILL.md must have a 'description' field");
            assertTrue(content.contains("tags:"), "SKILL.md must have a 'tags' field");
            assertTrue(content.contains("jamjet-durability-patterns"),
                    "name must be 'jamjet-durability-patterns'");
        }
    }

    @Test
    void allReferenceDocsExist() {
        String[] references = {
                "META-INF/skills/references/durable-agent-pattern.md",
                "META-INF/skills/references/crash-recovery-pattern.md",
                "META-INF/skills/references/audit-trail-pattern.md",
                "META-INF/skills/references/human-in-the-loop.md",
                "META-INF/skills/references/replay-testing.md",
                "META-INF/skills/references/spring-ai-integration.md"
        };

        for (String ref : references) {
            assertNotNull(
                    getClass().getClassLoader().getResourceAsStream(ref),
                    "Reference doc must exist: " + ref);
        }
    }
}
