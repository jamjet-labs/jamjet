package dev.jamjet.client;

/**
 * Thrown when the JamJet runtime returns a non-2xx HTTP response.
 */
public final class JamjetApiException extends RuntimeException {

    private final int statusCode;
    private final String body;
    private final String path;

    public JamjetApiException(int statusCode, String body, String path) {
        this(statusCode, body, path, "JamJet API error " + statusCode + " on " + path);
    }

    /** Construct with an explicit message, for responses that are wrong in a known way. */
    public JamjetApiException(int statusCode, String body, String path, String message) {
        super(message);
        this.statusCode = statusCode;
        this.body = body;
        this.path = path;
    }

    /** HTTP status code returned by the runtime. */
    public int statusCode() {
        return statusCode;
    }

    /** Raw response body from the runtime. */
    public String body() {
        return body;
    }

    /** The request path that triggered the error. */
    public String path() {
        return path;
    }
}
