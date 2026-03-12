//! Secret management — env-var backed with `${SECRET_NAME}` expansion (G2.3).
//!
//! Secrets are referenced in workflow payloads as `${MY_SECRET}`. At execution
//! time, the worker or API resolves them from environment variables. A pluggable
//! `SecretBackend` trait allows future integration with Vault, AWS Secrets Manager, etc.
//!
//! ## Redaction (G2.4)
//! `redact_value()` replaces resolved secret values with `[REDACTED]` before
//! they appear in logs or traces.

// ── Secret backend trait ──────────────────────────────────────────────────────

pub trait SecretBackend: Send + Sync {
    /// Resolve a named secret to its plaintext value.
    fn get(&self, name: &str) -> Option<String>;
}

// ── Env-var backend ───────────────────────────────────────────────────────────

pub struct EnvSecretBackend;

impl SecretBackend for EnvSecretBackend {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

// ── File-based backend (K8s mounted secrets, Docker secrets) ─────────────────

/// Reads secrets from a directory of files (one file per secret).
///
/// Designed for Kubernetes mounted secrets (`/var/run/secrets/`) and Docker
/// secrets (`/run/secrets/`). Each file name is the secret name and the file
/// content is the secret value.
pub struct FileSecretBackend {
    dir: std::path::PathBuf,
}

impl FileSecretBackend {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Create from `JAMJET_SECRETS_DIR` env var, or return None.
    pub fn from_env() -> Option<Self> {
        std::env::var("JAMJET_SECRETS_DIR")
            .ok()
            .map(|d| Self::new(d))
    }
}

impl SecretBackend for FileSecretBackend {
    fn get(&self, name: &str) -> Option<String> {
        // Prevent path traversal
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return None;
        }
        let path = self.dir.join(name);
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }
}

// ── HashiCorp Vault backend ──────────────────────────────────────────────────

/// Reads secrets from HashiCorp Vault's KV v2 engine via the `vault` CLI.
///
/// Requires the `vault` CLI to be on PATH and authenticated (e.g., via
/// `VAULT_ADDR` and `VAULT_TOKEN` env vars). Secret names are mapped to
/// Vault paths as `{mount}/{prefix}/{name}`.
pub struct VaultSecretBackend {
    /// KV mount path (e.g., "secret").
    mount: String,
    /// Key prefix within the mount (e.g., "jamjet/production").
    prefix: String,
    /// Field name within the Vault secret. Default: "value".
    field: String,
}

impl VaultSecretBackend {
    pub fn new(mount: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            mount: mount.into(),
            prefix: prefix.into(),
            field: "value".to_string(),
        }
    }

    /// Create from env vars: `VAULT_SECRET_MOUNT`, `VAULT_SECRET_PREFIX`.
    pub fn from_env() -> Option<Self> {
        let mount = std::env::var("VAULT_SECRET_MOUNT").unwrap_or_else(|_| "secret".to_string());
        let prefix = std::env::var("VAULT_SECRET_PREFIX").ok()?;
        Some(Self::new(mount, prefix))
    }
}

impl SecretBackend for VaultSecretBackend {
    fn get(&self, name: &str) -> Option<String> {
        // Prevent path traversal
        if name.contains("..") {
            return None;
        }
        let path = format!("{}/{}/{}", self.mount, self.prefix, name);
        let output = std::process::Command::new("vault")
            .args(["kv", "get", "-field", &self.field, &path])
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            None
        }
    }
}

// ── AWS Secrets Manager backend ──────────────────────────────────────────────

/// Reads secrets from AWS Secrets Manager via the `aws` CLI.
///
/// Requires the `aws` CLI to be configured (credentials, region, etc.).
/// Secret names are used directly as AWS secret IDs, optionally with a prefix.
pub struct AwsSecretBackend {
    /// Optional prefix prepended to all secret names (e.g., "jamjet/production/").
    prefix: String,
}

impl AwsSecretBackend {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// Create from `AWS_SECRET_PREFIX` env var.
    pub fn from_env() -> Option<Self> {
        let prefix = std::env::var("AWS_SECRET_PREFIX").unwrap_or_default();
        // Only return if AWS credentials are likely configured
        if std::env::var("AWS_DEFAULT_REGION").is_ok()
            || std::env::var("AWS_REGION").is_ok()
            || std::env::var("AWS_PROFILE").is_ok()
        {
            Some(Self::new(prefix))
        } else {
            None
        }
    }
}

impl SecretBackend for AwsSecretBackend {
    fn get(&self, name: &str) -> Option<String> {
        let secret_id = format!("{}{}", self.prefix, name);
        let output = std::process::Command::new("aws")
            .args([
                "secretsmanager",
                "get-secret-value",
                "--secret-id",
                &secret_id,
                "--query",
                "SecretString",
                "--output",
                "text",
            ])
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        } else {
            None
        }
    }
}

// ── Composite backend (chaining) ────────────────────────────────────────────

/// Tries multiple backends in order, returning the first match.
pub struct CompositeSecretBackend {
    backends: Vec<Box<dyn SecretBackend>>,
}

impl CompositeSecretBackend {
    pub fn new(backends: Vec<Box<dyn SecretBackend>>) -> Self {
        Self { backends }
    }

    /// Build the default backend stack from environment configuration.
    ///
    /// Priority: Vault > AWS Secrets Manager > File > Env vars (always present).
    pub fn from_env() -> Self {
        let mut backends: Vec<Box<dyn SecretBackend>> = Vec::new();

        if let Some(vault) = VaultSecretBackend::from_env() {
            backends.push(Box::new(vault));
        }
        if let Some(aws) = AwsSecretBackend::from_env() {
            backends.push(Box::new(aws));
        }
        if let Some(file) = FileSecretBackend::from_env() {
            backends.push(Box::new(file));
        }
        backends.push(Box::new(EnvSecretBackend));

        Self { backends }
    }
}

impl SecretBackend for CompositeSecretBackend {
    fn get(&self, name: &str) -> Option<String> {
        for backend in &self.backends {
            if let Some(value) = backend.get(name) {
                return Some(value);
            }
        }
        None
    }
}

// ── Secret expansion ──────────────────────────────────────────────────────────

/// Expand `${SECRET_NAME}` placeholders in a string using the given backend.
/// Unresolvable secrets are left as-is.
pub fn expand(s: &str, backend: &dyn SecretBackend) -> String {
    let mut result = s.to_string();
    let mut pos = 0;

    while let Some(start) = result[pos..].find("${") {
        let abs_start = pos + start;
        if let Some(end) = result[abs_start..].find('}') {
            let abs_end = abs_start + end;
            let name = &result[abs_start + 2..abs_end];
            if let Some(value) = backend.get(name) {
                result = format!(
                    "{}{}{}",
                    &result[..abs_start],
                    value,
                    &result[abs_end + 1..]
                );
                // Don't advance past the substitution — the value itself might contain ${}
                // but to avoid infinite loops just advance past what we replaced.
                pos = abs_start + value.len();
            } else {
                pos = abs_end + 1;
            }
        } else {
            break;
        }
    }

    result
}

/// Expand all string values in a JSON object recursively.
pub fn expand_json(value: &serde_json::Value, backend: &dyn SecretBackend) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(expand(s, backend)),
        serde_json::Value::Object(map) => {
            let expanded: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), expand_json(v, backend)))
                .collect();
            serde_json::Value::Object(expanded)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| expand_json(v, backend)).collect())
        }
        other => other.clone(),
    }
}

// ── Secret redaction ──────────────────────────────────────────────────────────

/// Build a redaction map from known secret names (reads them to find their values,
/// then replaces those values with `[REDACTED]` in any string).
pub struct Redactor {
    known_values: Vec<String>,
}

impl Redactor {
    /// Create a redactor that will mask the values of all listed secret names.
    pub fn from_names(names: &[&str], backend: &dyn SecretBackend) -> Self {
        let known_values = names
            .iter()
            .filter_map(|n| backend.get(n))
            .filter(|v| v.len() >= 8) // don't redact very short values (false positives)
            .collect();
        Self { known_values }
    }

    /// Redact all known secret values from a string.
    pub fn redact_str(&self, s: &str) -> String {
        let mut result = s.to_string();
        for secret in &self.known_values {
            result = result.replace(secret.as_str(), "[REDACTED]");
        }
        result
    }

    /// Redact all known secret values from a JSON value.
    pub fn redact_json(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.redact_str(s)),
            serde_json::Value::Object(map) => {
                let redacted = map
                    .iter()
                    .map(|(k, v)| (k.clone(), self.redact_json(v)))
                    .collect();
                serde_json::Value::Object(redacted)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(|v| self.redact_json(v)).collect())
            }
            other => other.clone(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct MapBackend(HashMap<String, String>);
    impl SecretBackend for MapBackend {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn backend(pairs: &[(&str, &str)]) -> MapBackend {
        MapBackend(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_expand_simple() {
        let b = backend(&[("MY_KEY", "secret123")]);
        assert_eq!(expand("Bearer ${MY_KEY}", &b), "Bearer secret123");
    }

    #[test]
    fn test_expand_missing() {
        let b = backend(&[]);
        assert_eq!(expand("Bearer ${MISSING}", &b), "Bearer ${MISSING}");
    }

    #[test]
    fn test_expand_json() {
        let b = backend(&[("DB_PASS", "pa$$word")]);
        let v = serde_json::json!({ "password": "${DB_PASS}", "user": "admin" });
        let expanded = expand_json(&v, &b);
        assert_eq!(expanded["password"], "pa$$word");
        assert_eq!(expanded["user"], "admin");
    }

    #[test]
    fn test_redactor() {
        let b = backend(&[("API_KEY", "super-secret-value")]);
        let r = Redactor::from_names(&["API_KEY"], &b);
        assert_eq!(
            r.redact_str("key is super-secret-value!"),
            "key is [REDACTED]!"
        );
    }

    #[test]
    fn test_file_backend() {
        let dir = std::env::temp_dir().join("jamjet-secrets-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("DB_PASSWORD"), "hunter2\n").unwrap();
        std::fs::write(dir.join("API_KEY"), "  sk-abc123  ").unwrap();

        let b = FileSecretBackend::new(&dir);
        assert_eq!(b.get("DB_PASSWORD"), Some("hunter2".to_string()));
        assert_eq!(b.get("API_KEY"), Some("sk-abc123".to_string()));
        assert_eq!(b.get("MISSING"), None);

        // Path traversal prevention
        assert_eq!(b.get("../etc/passwd"), None);
        assert_eq!(b.get("foo/bar"), None);

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_composite_backend_priority() {
        let high = backend(&[("SHARED", "from-high"), ("HIGH_ONLY", "h")]);
        let low = backend(&[("SHARED", "from-low"), ("LOW_ONLY", "l")]);
        let composite = CompositeSecretBackend::new(vec![Box::new(high), Box::new(low)]);

        // High-priority backend wins for shared key
        assert_eq!(composite.get("SHARED"), Some("from-high".to_string()));
        // Falls through to low-priority for unique keys
        assert_eq!(composite.get("LOW_ONLY"), Some("l".to_string()));
        assert_eq!(composite.get("HIGH_ONLY"), Some("h".to_string()));
        assert_eq!(composite.get("MISSING"), None);
    }
}
