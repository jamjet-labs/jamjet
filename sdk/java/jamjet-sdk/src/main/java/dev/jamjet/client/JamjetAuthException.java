package dev.jamjet.client;

/**
 * Thrown when the JamJet runtime returns a 401 or 403 response.
 *
 * <p>Check that {@code JAMJET_TOKEN} is set correctly or pass a token via
 * {@link ClientConfig.Builder#apiToken(String)}.
 */
public final class JamjetAuthException extends RuntimeException {

    private final int statusCode;
    private final String path;

    public JamjetAuthException(int statusCode, String body, String path) {
        super("JamJet authentication failed (" + statusCode + ") on " + path
                + ". Check your API token.");
        this.statusCode = statusCode;
        this.path = path;
    }

    /** HTTP status code (401 or 403). */
    public int statusCode() {
        return statusCode;
    }

    /** The request path that triggered the auth failure. */
    public String path() {
        return path;
    }
}
