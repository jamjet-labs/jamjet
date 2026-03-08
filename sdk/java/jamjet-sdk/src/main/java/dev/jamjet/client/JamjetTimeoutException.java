package dev.jamjet.client;

/**
 * Thrown when an HTTP request to the JamJet runtime times out.
 */
public final class JamjetTimeoutException extends RuntimeException {

    private final String path;

    public JamjetTimeoutException(String path, Throwable cause) {
        super("JamJet request timed out for path: " + path, cause);
        this.path = path;
    }

    /** The request path that timed out. */
    public String path() {
        return path;
    }
}
