//! Building the HTTP client the LLM and embedding providers use.
//!
//! `reqwest::Client::new()` panics when the TLS backend cannot start. On Linux
//! that happens on a system with no CA bundle: reqwest's rustls backend loads
//! the operating system's trust store when a client is built, and refuses an
//! empty one, even for a plain-HTTP URL such as a local Ollama. A server
//! should call [`client`] at startup, so it can exit with that explanation
//! before any provider is built.

use std::time::Duration;

/// Build an HTTP client, or explain why none can be built.
pub fn client(timeout: Option<Duration>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    builder.build().map_err(|e| {
        let mut msg = format!("cannot build an HTTP client: {e}");
        let mut source = std::error::Error::source(&e);
        while let Some(s) = source {
            msg.push_str(&format!(": {s}"));
            source = s.source();
        }
        msg.push_str(
            ". TLS verification uses the system CA bundle; on Linux install \
             ca-certificates, or set SSL_CERT_FILE to a PEM bundle",
        );
        msg
    })
}

/// [`client`] for the providers' constructors, which cannot return an error.
pub(crate) fn client_or_panic(timeout: Option<Duration>) -> reqwest::Client {
    client(timeout).unwrap_or_else(|e| panic!("{e}"))
}
