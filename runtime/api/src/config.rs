use serde::{Deserialize, Serialize};

/// Turns authentication off for local development. A boolean switch
/// ([`jamjet_core::flag`]).
pub const DEV_MODE_ENV: &str = "JAMJET_DEV_MODE";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Host address the API server binds to.
    /// Defaults to 127.0.0.1 (localhost only). Set JAMJET_BIND=0.0.0.0 to expose to the network.
    pub bind: String,
    /// Port the API server listens on.
    pub port: u16,
    /// Database URL. If unset, defaults to SQLite local mode.
    pub database_url: Option<String>,
    /// Log level (trace, debug, info, warn, error).
    pub log_level: String,
    /// Whether to run in dev mode (SQLite, embedded runtime).
    pub dev_mode: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: std::env::var("JAMJET_BIND").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("JAMJET_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(7700),
            database_url: None,
            log_level: "info".into(),
            // `jamjet-server` refuses a value that is not a boolean before it
            // gets here (`startup::check_flags`); a library caller reads it as
            // off, which keeps authentication on.
            dev_mode: jamjet_core::flag::env_flag(DEV_MODE_ENV).unwrap_or(false),
        }
    }
}
