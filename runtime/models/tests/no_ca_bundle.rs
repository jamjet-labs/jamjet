//! On Linux, building a reqwest client loads the system trust store, and with
//! none the build fails, so `Client::new()` panics. Startup has to report that
//! instead. `SSL_CERT_FILE` naming an empty file looks, to the TLS backend,
//! like a system with no CA bundle.
//!
//! Keep this the only test in the file: it sets process environment.
#![cfg(target_os = "linux")]

#[tokio::test]
async fn startup_reports_a_missing_ca_bundle_instead_of_panicking() {
    let empty = std::env::temp_dir().join(format!("jamjet-no-ca-{}.pem", std::process::id()));
    std::fs::write(&empty, b"").unwrap();
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &empty);
        std::env::remove_var("SSL_CERT_DIR");
        std::env::remove_var("JAMJET_MODEL_SEAM_URL");
    }

    // Control: this is the condition under which startup used to panic.
    assert!(
        std::panic::catch_unwind(reqwest::Client::new).is_err(),
        "Client::new() should panic when no CA certificate loads"
    );

    let err = jamjet_models::http::client().unwrap_err().to_string();
    assert!(
        err.contains("ca-certificates") && err.contains("SSL_CERT_FILE"),
        "{err}"
    );
    let err = jamjet_models::registry::registry_from_env_checked()
        .await
        .err()
        .expect("startup must fail without a CA bundle")
        .to_string();
    assert!(err.contains("ca-certificates"), "{err}");
}
