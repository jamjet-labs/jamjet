//! On Linux, building a reqwest client loads the system trust store, and with
//! none the build fails, so `Client::new()` panics. engram-server checks with
//! `engram::http::client` before building any provider, and the providers'
//! constructors panic with the same explanation. `SSL_CERT_FILE` naming an
//! empty file looks, to the TLS backend, like a system with no CA bundle.
//!
//! Keep this the only test in the file: it sets process environment.
#![cfg(target_os = "linux")]

#[test]
fn a_missing_ca_bundle_is_explained() {
    let empty = std::env::temp_dir().join(format!("engram-no-ca-{}.pem", std::process::id()));
    std::fs::write(&empty, b"").unwrap();
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &empty);
        std::env::remove_var("SSL_CERT_DIR");
    }

    // Control: the condition is real.
    assert!(std::panic::catch_unwind(reqwest::Client::new).is_err());

    let err = engram::http::client(None).unwrap_err();
    assert!(
        err.contains("ca-certificates") && err.contains("SSL_CERT_FILE"),
        "{err}"
    );

    let Err(panic) = std::panic::catch_unwind(engram::llm_ollama::OllamaLlmClient::new) else {
        panic!("a provider cannot be built without a client");
    };
    let text = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(text.contains("ca-certificates"), "{text}");
}
