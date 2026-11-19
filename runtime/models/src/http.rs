//! The HTTP client check a server runs at startup.
//!
//! `reqwest::Client::new()` panics when the TLS backend cannot start. On Linux
//! that happens on a system with no CA bundle: reqwest's rustls backend loads
//! the operating system's trust store when a client is built, and refuses an
//! empty one, even for a plain-HTTP URL such as a local Ollama. Building
//! through [`client`] turns that into an error that says what to install.

use crate::adapter::ModelError;
use std::time::Duration;

/// How long any client here waits to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one model call may take, end to end, unless
/// [`MODEL_TIMEOUT_ENV`] says otherwise. The adapters do not stream, so this
/// bounds the whole response.
pub const DEFAULT_MODEL_TIMEOUT: Duration = Duration::from_secs(300);

/// Overrides [`DEFAULT_MODEL_TIMEOUT`], in whole seconds (at least 1).
pub const MODEL_TIMEOUT_ENV: &str = "JAMJET_MODEL_TIMEOUT_SECS";

/// The total timeout of a startup probe made with [`client`].
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Build an HTTP client for startup checks, or explain why none can be built.
pub fn client() -> Result<reqwest::Client, ModelError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|e| ModelError::Network(cannot_build(&e)))
}

/// The model call timeout: [`MODEL_TIMEOUT_ENV`], or
/// [`DEFAULT_MODEL_TIMEOUT`] when unset. A value that is not a whole number of
/// seconds of at least 1 is an error, which `registry_from_env_checked`
/// reports at startup.
pub fn model_timeout() -> Result<Duration, ModelError> {
    match std::env::var(MODEL_TIMEOUT_ENV) {
        Err(_) => Ok(DEFAULT_MODEL_TIMEOUT),
        Ok(raw) => parse_model_timeout(&raw),
    }
}

fn parse_model_timeout(raw: &str) -> Result<Duration, ModelError> {
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs >= 1 => Ok(Duration::from_secs(secs)),
        _ => Err(ModelError::Network(format!(
            "{MODEL_TIMEOUT_ENV}={raw:?} is not a whole number of seconds of at least 1"
        ))),
    }
}

/// The client a model adapter calls its provider with: [`CONNECT_TIMEOUT`]
/// to connect and [`model_timeout`] for the whole call. Every adapter used
/// `reqwest::Client::new()`, which has neither, so a provider that accepted
/// the connection and never answered held the node forever.
///
/// Panics, as `reqwest::Client::new()` did, when no client can be built;
/// `registry_from_env_checked` turns that case into a startup error first.
pub fn model_client() -> reqwest::Client {
    model_client_with(model_timeout().unwrap_or(DEFAULT_MODEL_TIMEOUT))
}

/// [`model_client`] with an explicit total timeout.
pub fn model_client_with(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .unwrap_or_else(|e| panic!("{}", cannot_build(&e)))
}

/// The message for a client that cannot be built.
pub fn cannot_build(e: &reqwest::Error) -> String {
    let mut msg = format!("cannot build an HTTP client: {e}");
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(&format!(": {s}"));
        source = s.source();
    }
    msg.push_str(
        ". TLS verification uses the system CA bundle; on Linux install \
         ca-certificates, or set SSL_CERT_FILE to a PEM bundle",
    );
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_timeout_is_whole_seconds_of_at_least_one() {
        assert_eq!(parse_model_timeout("45").unwrap(), Duration::from_secs(45));
        assert_eq!(
            parse_model_timeout(" 600 ").unwrap(),
            Duration::from_secs(600)
        );
        for bad in ["0", "-1", "1.5", "", "ten"] {
            assert!(parse_model_timeout(bad).is_err(), "{bad:?}");
        }
    }
}
