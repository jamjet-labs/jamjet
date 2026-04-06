package dev.jamjet.engram;

/**
 * Configuration for the Engram HTTP client.
 *
 * <p>Reads {@code ENGRAM_TOKEN} from the environment if {@code apiToken} is {@code null}.
 * All fields have sensible defaults for local development.
 */
public final class EngramConfig {

    private final String baseUrl;
    private final String apiToken;
    private final int timeoutSeconds;

    private EngramConfig(Builder builder) {
        this.baseUrl = builder.baseUrl;
        this.timeoutSeconds = builder.timeoutSeconds;

        String token = builder.apiToken;
        if (token == null || token.isBlank()) {
            token = System.getenv("ENGRAM_TOKEN");
        }
        this.apiToken = token;
    }

    /** Base URL of the Engram REST API. Defaults to {@code http://localhost:9090}. */
    public String baseUrl() {
        return baseUrl;
    }

    /** API token for authentication. May be {@code null}. */
    public String apiToken() {
        return apiToken;
    }

    /** HTTP request timeout in seconds. Defaults to 30. */
    public int timeoutSeconds() {
        return timeoutSeconds;
    }

    public static Builder builder() {
        return new Builder();
    }

    public static EngramConfig defaults() {
        return builder().build();
    }

    public static final class Builder {

        private String baseUrl = "http://localhost:9090";
        private String apiToken = null;
        private int timeoutSeconds = 30;

        private Builder() {}

        public Builder baseUrl(String baseUrl) {
            if (baseUrl == null || baseUrl.isBlank()) {
                throw new IllegalArgumentException("baseUrl must not be blank");
            }
            this.baseUrl = baseUrl.stripTrailing().replaceAll("/+$", "");
            return this;
        }

        public Builder apiToken(String apiToken) {
            this.apiToken = apiToken;
            return this;
        }

        public Builder timeoutSeconds(int timeoutSeconds) {
            if (timeoutSeconds < 1) {
                throw new IllegalArgumentException("timeoutSeconds must be >= 1");
            }
            this.timeoutSeconds = timeoutSeconds;
            return this;
        }

        public EngramConfig build() {
            return new EngramConfig(this);
        }
    }
}
