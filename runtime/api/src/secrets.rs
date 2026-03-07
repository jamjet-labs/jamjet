//! Secret management — env-var backed with `${SECRET_NAME}` expansion (G2.3).
//!
//! Secrets are referenced in workflow payloads as `${MY_SECRET}`. At execution
//! time, the worker or API resolves them from environment variables. A pluggable
//! `SecretBackend` trait allows future integration with Vault, AWS Secrets Manager, etc.
//!
//! ## Redaction (G2.4)
//! `redact_value()` replaces resolved secret values with `[REDACTED]` before
//! they appear in logs or traces.

use std::collections::HashMap;

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
}
