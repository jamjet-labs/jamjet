//! The HTTP client check a server runs at startup.
//!
//! `reqwest::Client::new()` panics when the TLS backend cannot start. On Linux
//! that happens on a system with no CA bundle: reqwest's rustls backend loads
//! the operating system's trust store when a client is built, and refuses an
//! empty one, even for a plain-HTTP URL such as a local Ollama. Building
//! through [`client`] turns that into an error that says what to install.

use crate::adapter::ModelError;

/// Build an HTTP client, or explain why none can be built.
pub fn client() -> Result<reqwest::Client, ModelError> {
    reqwest::Client::builder()
        .build()
        .map_err(|e| ModelError::Network(cannot_build(&e)))
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
