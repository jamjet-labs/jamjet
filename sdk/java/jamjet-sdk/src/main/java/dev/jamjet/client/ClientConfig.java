package dev.jamjet.client;

/**
 * Configuration for the JamJet HTTP client.
 *
 * <p>Reads {@code JAMJET_TOKEN} from the environment if {@code apiToken} is {@code null}.
 * All fields have sensible defaults for local development.
 */
public final class ClientConfig {

    private final String baseUrl;
    private final String apiToken;
    private final int timeoutSeconds;

    private ClientConfig(Builder builder) {
        this.baseUrl = builder.baseUrl;
        this.timeoutSeconds = builder.timeoutSeconds;

        // Resolve token from env if not provided explicitly
        String token = builder.apiToken;
        if (token == null || token.isBlank()) {
            token = System.getenv("JAMJET_TOKEN");
        }
        this.apiToken = token;
    }

    /** Base URL of the JamJet runtime API. Defaults to {@code http://localhost:7700}. */
    public String baseUrl() {
        return baseUrl;
    }

    /** API token for authentication. May be {@code null} if the server is unauthenticated. */
    public String apiToken() {
        return apiToken;
    }

    /** HTTP request timeout in seconds. Defaults to 30. */
    public int timeoutSeconds() {
        return timeoutSeconds;
    }

    /** Return a new builder pre-populated with defaults. */
    public static Builder builder() {
        return new Builder();
    }

    /** Return a config with all defaults (local runtime, no auth). */
    public static ClientConfig defaults() {
        return builder().build();
    }

    @Override
    public String toString() {
        return "ClientConfig{baseUrl='" + baseUrl + "', timeoutSeconds=" + timeoutSeconds
                + ", hasToken=" + (apiToken != null) + "}";
    }

    public static final class Builder {

        private String baseUrl = "http://localhost:7700";
        private String apiToken = null;
        private int timeoutSeconds = 30;

        private Builder() {}

        /** Set the base URL of the JamJet runtime. */
        public Builder baseUrl(String baseUrl) {
            if (baseUrl == null || baseUrl.isBlank()) {
                throw new IllegalArgumentException("baseUrl must not be blank");
            }
            this.baseUrl = baseUrl.stripTrailing().replaceAll("/+$", "");
            return this;
        }

        /**
         * Set the API token explicitly. If not set (or blank), the builder will fall back to the
         * {@code JAMJET_TOKEN} environment variable.
         */
        public Builder apiToken(String apiToken) {
            this.apiToken = apiToken;
            return this;
        }

        /** Set the HTTP request timeout in seconds. Must be >= 1. */
        public Builder timeoutSeconds(int timeoutSeconds) {
            if (timeoutSeconds < 1) {
                throw new IllegalArgumentException("timeoutSeconds must be >= 1");
            }
            this.timeoutSeconds = timeoutSeconds;
            return this;
        }

        public ClientConfig build() {
            return new ClientConfig(this);
        }
    }
}
