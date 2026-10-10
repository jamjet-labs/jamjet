package dev.jamjet.client;

import com.fasterxml.jackson.databind.ObjectMapper;

/**
 * Thrown when the JamJet runtime returns a non-2xx HTTP response.
 */
public final class JamjetApiException extends RuntimeException {

    /** Longest runtime error text copied into the message; the full body stays on {@link #body()}. */
    static final int MAX_DETAIL_CHARS = 500;

    private static final ObjectMapper MAPPER = new ObjectMapper();

    private final int statusCode;
    private final String body;
    private final String path;

    public JamjetApiException(int statusCode, String body, String path) {
        this(statusCode, body, path, withDetail("JamJet API error " + statusCode + " on " + path, body));
    }

    /** Construct with an explicit message, for responses that are wrong in a known way. */
    public JamjetApiException(int statusCode, String body, String path, String message) {
        super(message);
        this.statusCode = statusCode;
        this.body = body;
        this.path = path;
    }

    /**
     * Append what the runtime said about the failure: its {@code {"error": "..."}} field
     * when the body has one, otherwise the raw body text, capped at
     * {@value #MAX_DETAIL_CHARS} characters.
     */
    static String withDetail(String message, String body) {
        var detail = detail(body);
        return detail.isEmpty() ? message : message + ": " + detail;
    }

    private static String detail(String body) {
        if (body == null || body.isBlank()) return "";
        var text = body;
        try {
            var node = MAPPER.readTree(body);
            if (node != null && node.path("error").isTextual() && !node.path("error").asText().isBlank()) {
                text = node.path("error").asText();
            }
        } catch (Exception ignored) {
            // Not JSON: use the raw text.
        }
        text = text.strip().replaceAll("\\s+", " ");
        if (text.length() > MAX_DETAIL_CHARS) {
            text = text.substring(0, MAX_DETAIL_CHARS) + "... (truncated)";
        }
        return text;
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
