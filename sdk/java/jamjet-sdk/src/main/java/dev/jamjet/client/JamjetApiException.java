package dev.jamjet.client;

/**
 * Thrown when the JamJet runtime returns a non-2xx HTTP response.
 */
public final class JamjetApiException extends RuntimeException {

    private final int statusCode;
    private final String body;
    private final String path;

    public JamjetApiException(int statusCode, String body, String path) {
        super("JamJet API error " + statusCode + " on " + path + ": " + body);
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
