package dev.jamjet;

import org.junit.jupiter.api.Test;

import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.stream.Stream;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * {@code dev.jamjet:jamjet-agent} (jamjet-runtime-java) owns the {@code dev.jamjet.agent}
 * package: its {@code Agent} is the governed one. If jamjet-sdk ships classes there too, the
 * two jars share fully qualified names ({@code dev.jamjet.agent.Agent},
 * {@code AgentResult}) and whichever jar comes first on the classpath wins, possibly the
 * ungoverned one. jamjet-sdk must ship nothing in that package.
 */
class NoClassesInJamjetAgentPackageTest {

    @Test
    void jamjetSdkShipsNothingInDevJamjetAgent() throws Exception {
        URI location = JamjetClient.class.getProtectionDomain().getCodeSource().getLocation().toURI();
        Path root = Path.of(location);
        List<String> found;
        if (Files.isDirectory(root)) {
            found = list(root.resolve("dev/jamjet/agent"));
        } else {
            try (FileSystem jar = FileSystems.newFileSystem(root, Map.of())) {
                found = list(jar.getPath("/dev/jamjet/agent"));
            }
        }
        assertEquals(List.of(), found);
    }

    private static List<String> list(Path dir) throws Exception {
        if (!Files.exists(dir)) {
            return List.of();
        }
        try (Stream<Path> files = Files.walk(dir)) {
            return files.filter(Files::isRegularFile).map(p -> p.getFileName().toString()).sorted().toList();
        }
    }
}
