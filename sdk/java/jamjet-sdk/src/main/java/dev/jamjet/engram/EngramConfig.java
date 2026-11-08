package dev.jamjet.engram;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;
import java.util.function.Function;

/**
 * Configuration for the Engram HTTP client.
 *
 * <p>When {@code apiToken} is not set, reads {@code ENGRAM_AUTH_TOKEN} from the environment,
 * the variable the Engram server reads. {@code ENGRAM_TOKEN} is still read as a deprecated
 * fallback, with a warning. All fields have sensible defaults for local development.
 */
public final class EngramConfig {

    private static final Logger log = LoggerFactory.getLogger(EngramConfig.class);
    private static final AtomicBoolean LEGACY_WARNED = new AtomicBoolean();

    /** The variable the Engram server reads its token from. */
    public static final String AUTH_TOKEN_ENV = "ENGRAM_AUTH_TOKEN";
    /** The variable this SDK used to read; still a fallback. */
    public static final String LEGACY_TOKEN_ENV = "ENGRAM_TOKEN";

    /** The warning for a token found only in {@link #LEGACY_TOKEN_ENV}. Never carries the token. */
    static final String LEGACY_WARNING = LEGACY_TOKEN_ENV + " is deprecated; set " + AUTH_TOKEN_ENV
            + ", the variable the Engram server reads.";

    private final String baseUrl;
    private final String apiToken;
    private final int timeoutSeconds;

    private EngramConfig(Builder builder) {
        this.baseUrl = builder.baseUrl;
        this.timeoutSeconds = builder.timeoutSeconds;

        this.apiToken = resolveToken(builder.apiToken, System::getenv, EngramConfig::warnLegacyOnce);
    }

    /**
     * The explicit token if set, else {@link #AUTH_TOKEN_ENV}, else {@link #LEGACY_TOKEN_ENV}
     * (calling {@code warn} with {@link #LEGACY_WARNING}). {@code null} when none is set.
     */
    static String resolveToken(String explicit, Function<String, String> env, Consumer<String> warn) {
        if (explicit != null && !explicit.isBlank()) {
            return explicit;
        }
        String token = env.apply(AUTH_TOKEN_ENV);
        if (token != null && !token.isBlank()) {
            return token;
        }
        token = env.apply(LEGACY_TOKEN_ENV);
        if (token != null && !token.isBlank()) {
            warn.accept(LEGACY_WARNING);
            return token;
        }
        return null;
    }

    private static void warnLegacyOnce(String message) {
        if (LEGACY_WARNED.compareAndSet(false, true)) {
            log.warn(message);
        }
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
