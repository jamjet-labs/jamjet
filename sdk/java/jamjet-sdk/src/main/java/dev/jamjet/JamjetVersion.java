package dev.jamjet;

import java.io.IOException;
import java.io.InputStream;
import java.util.Properties;

/**
 * The version of this SDK build, as Maven stamped it into
 * {@code dev/jamjet/jamjet-sdk-version.properties} at build time.
 */
public final class JamjetVersion {

    private static final String VERSION = read(JamjetVersion.class, "jamjet-sdk-version.properties");

    private JamjetVersion() {}

    /** The SDK version, such as {@code 0.5.1}, or {@code unknown} if the build did not stamp it. */
    public static String get() {
        return VERSION;
    }

    /**
     * Read {@code version} from a properties resource next to {@code anchor}.
     * Returns {@code unknown} when the resource is missing or was not filtered.
     */
    public static String read(Class<?> anchor, String resource) {
        try (InputStream in = anchor.getResourceAsStream(resource)) {
            if (in == null) return "unknown";
            var props = new Properties();
            props.load(in);
            var v = props.getProperty("version", "").strip();
            return v.isEmpty() || v.startsWith("${") ? "unknown" : v;
        } catch (IOException e) {
            return "unknown";
        }
    }
}
