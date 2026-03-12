//! PII detection and redaction engine.
//!
//! Provides pattern-based PII detection (email, SSN, credit card, etc.) and
//! JSON field-path PII tagging. Used by the audit enricher and telemetry capture
//! to sanitize sensitive data before persistence.

use jamjet_ir::workflow::DataPolicyIr;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How to handle detected PII values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionMode {
    /// Replace with `[REDACTED]`.
    Mask,
    /// Replace with SHA-256 hex digest (pseudonymization).
    Hash,
    /// Remove the key entirely from JSON objects.
    Remove,
}

impl RedactionMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "hash" => Self::Hash,
            "remove" => Self::Remove,
            _ => Self::Mask,
        }
    }
}

/// A compiled PII pattern detector.
struct Detector {
    _name: String,
    regex: Regex,
}

/// Policy-driven PII redactor.
///
/// Constructed from a `DataPolicyIr`, compiling all regex patterns once.
/// Thread-safe and reusable across requests.
pub struct PiiRedactor {
    detectors: Vec<Detector>,
    /// Dot-separated field paths to redact (e.g. `"patient.ssn"`).
    field_paths: Vec<Vec<String>>,
    mode: RedactionMode,
}

impl PiiRedactor {
    /// Build a redactor from a `DataPolicyIr`.
    pub fn from_policy(policy: &DataPolicyIr) -> Self {
        let mut detectors = Vec::new();

        for name in &policy.pii_detectors {
            if let Some(pattern) = builtin_pattern(name) {
                if let Ok(regex) = Regex::new(pattern) {
                    detectors.push(Detector {
                        _name: name.clone(),
                        regex,
                    });
                }
            }
        }

        // Parse field paths: "$.patient.ssn" -> ["patient", "ssn"]
        let field_paths = policy
            .pii_fields
            .iter()
            .map(|p| {
                let stripped = p.strip_prefix("$.").unwrap_or(p);
                stripped.split('.').map(|s| s.to_string()).collect()
            })
            .collect();

        Self {
            detectors,
            field_paths,
            mode: RedactionMode::parse(&policy.redaction_mode),
        }
    }

    /// Returns true if this redactor has any active rules.
    pub fn is_active(&self) -> bool {
        !self.detectors.is_empty() || !self.field_paths.is_empty()
    }

    /// Redact PII from a plain string using pattern detectors.
    pub fn redact_str(&self, s: &str) -> String {
        let mut result = s.to_string();
        for detector in &self.detectors {
            result = detector
                .regex
                .replace_all(&result, |caps: &regex::Captures| {
                    self.redact_value(caps.get(0).map(|m| m.as_str()).unwrap_or(""))
                })
                .to_string();
        }
        result
    }

    /// Redact PII from a JSON value (both field paths and pattern detectors).
    pub fn redact_json(&self, value: &Value) -> Value {
        let mut result = value.clone();

        // 1. Field-path redaction
        for path in &self.field_paths {
            redact_at_path(&mut result, path, &self.mode);
        }

        // 2. Pattern-based redaction on all string values
        if !self.detectors.is_empty() {
            redact_strings_recursive(&mut result, self);
        }

        result
    }

    /// Apply the configured redaction mode to a single value.
    fn redact_value(&self, original: &str) -> String {
        match &self.mode {
            RedactionMode::Mask => "[REDACTED]".to_string(),
            RedactionMode::Hash => {
                let hash = Sha256::digest(original.as_bytes());
                format!("[HASH:{}]", hex_encode(&hash[..8]))
            }
            RedactionMode::Remove => String::new(),
        }
    }
}

/// Navigate a JSON value by dot-path and redact the leaf.
fn redact_at_path(value: &mut Value, path: &[String], mode: &RedactionMode) {
    if path.is_empty() {
        return;
    }

    if path.len() == 1 {
        // Leaf: redact this key
        let key = &path[0];
        if key == "*" {
            // Wildcard: redact all keys at this level
            if let Value::Object(map) = value {
                let keys: Vec<String> = map.keys().cloned().collect();
                for k in keys {
                    apply_redaction(map, &k, mode);
                }
            }
        } else if let Value::Object(map) = value {
            apply_redaction(map, key, mode);
        }
        return;
    }

    let key = &path[0];
    let rest = &path[1..];

    if key == "*" {
        // Wildcard: descend into all keys
        if let Value::Object(map) = value {
            for v in map.values_mut() {
                redact_at_path(v, rest, mode);
            }
        }
    } else if let Value::Object(map) = value {
        if let Some(child) = map.get_mut(key.as_str()) {
            redact_at_path(child, rest, mode);
        }
    }
}

fn apply_redaction(map: &mut serde_json::Map<String, Value>, key: &str, mode: &RedactionMode) {
    match mode {
        RedactionMode::Remove => {
            map.remove(key);
        }
        RedactionMode::Mask => {
            if map.contains_key(key) {
                map.insert(key.to_string(), Value::String("[REDACTED]".to_string()));
            }
        }
        RedactionMode::Hash => {
            if let Some(val) = map.get(key) {
                let original = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let hash = Sha256::digest(original.as_bytes());
                map.insert(
                    key.to_string(),
                    Value::String(format!("[HASH:{}]", hex_encode(&hash[..8]))),
                );
            }
        }
    }
}

/// Recursively walk JSON and apply pattern detectors to all string values.
fn redact_strings_recursive(value: &mut Value, redactor: &PiiRedactor) {
    match value {
        Value::String(s) => {
            *s = redactor.redact_str(s);
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                redact_strings_recursive(v, redactor);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_strings_recursive(v, redactor);
            }
        }
        _ => {}
    }
}

/// Return the regex pattern for a built-in PII detector name.
fn builtin_pattern(name: &str) -> Option<&'static str> {
    match name {
        "email" => Some(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"),
        "ssn" => Some(r"\b\d{3}-\d{2}-\d{4}\b"),
        "credit_card" => Some(r"\b\d{4}[- ]?\d{4}[- ]?\d{4}[- ]?\d{4}\b"),
        "phone" => Some(r"\b\+?1?[-.\s]?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b"),
        "ip_address" => Some(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_detectors(detectors: &[&str]) -> DataPolicyIr {
        DataPolicyIr {
            pii_detectors: detectors.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn policy_with_fields(fields: &[&str]) -> DataPolicyIr {
        DataPolicyIr {
            pii_fields: fields.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn mask_email_in_string() {
        let r = PiiRedactor::from_policy(&policy_with_detectors(&["email"]));
        let result = r.redact_str("Contact user@example.com for details");
        assert_eq!(result, "Contact [REDACTED] for details");
    }

    #[test]
    fn mask_ssn_in_string() {
        let r = PiiRedactor::from_policy(&policy_with_detectors(&["ssn"]));
        let result = r.redact_str("SSN is 123-45-6789");
        assert_eq!(result, "SSN is [REDACTED]");
    }

    #[test]
    fn mask_credit_card_in_string() {
        let r = PiiRedactor::from_policy(&policy_with_detectors(&["credit_card"]));
        let result = r.redact_str("Card: 4111-1111-1111-1111");
        assert_eq!(result, "Card: [REDACTED]");
    }

    #[test]
    fn hash_mode_produces_stable_hash() {
        let policy = DataPolicyIr {
            pii_detectors: vec!["email".to_string()],
            redaction_mode: "hash".to_string(),
            ..Default::default()
        };
        let r = PiiRedactor::from_policy(&policy);
        let result1 = r.redact_str("user@example.com");
        let result2 = r.redact_str("user@example.com");
        assert_eq!(result1, result2);
        assert!(result1.starts_with("[HASH:"));
        assert_ne!(result1, "user@example.com");
    }

    #[test]
    fn field_path_redaction_in_json() {
        let r = PiiRedactor::from_policy(&policy_with_fields(&["$.patient.ssn", "$.user.email"]));
        let input = serde_json::json!({
            "patient": {"ssn": "123-45-6789", "name": "Alice"},
            "user": {"email": "alice@example.com", "role": "admin"}
        });
        let result = r.redact_json(&input);
        assert_eq!(result["patient"]["ssn"], "[REDACTED]");
        assert_eq!(result["patient"]["name"], "Alice");
        assert_eq!(result["user"]["email"], "[REDACTED]");
        assert_eq!(result["user"]["role"], "admin");
    }

    #[test]
    fn wildcard_field_path() {
        let r = PiiRedactor::from_policy(&policy_with_fields(&["$.*.secret"]));
        let input = serde_json::json!({
            "a": {"secret": "s1", "public": "p1"},
            "b": {"secret": "s2", "public": "p2"}
        });
        let result = r.redact_json(&input);
        assert_eq!(result["a"]["secret"], "[REDACTED]");
        assert_eq!(result["b"]["secret"], "[REDACTED]");
        assert_eq!(result["a"]["public"], "p1");
    }

    #[test]
    fn remove_mode_deletes_key() {
        let policy = DataPolicyIr {
            pii_fields: vec!["$.secret".to_string()],
            redaction_mode: "remove".to_string(),
            ..Default::default()
        };
        let r = PiiRedactor::from_policy(&policy);
        let input = serde_json::json!({"secret": "val", "public": "ok"});
        let result = r.redact_json(&input);
        assert!(result.get("secret").is_none());
        assert_eq!(result["public"], "ok");
    }

    #[test]
    fn combined_field_and_pattern_detection() {
        let policy = DataPolicyIr {
            pii_fields: vec!["$.user.ssn".to_string()],
            pii_detectors: vec!["email".to_string()],
            ..Default::default()
        };
        let r = PiiRedactor::from_policy(&policy);
        let input = serde_json::json!({
            "user": {"ssn": "123-45-6789", "bio": "Reach me at alice@test.com"},
            "notes": "Contact bob@test.com"
        });
        let result = r.redact_json(&input);
        assert_eq!(result["user"]["ssn"], "[REDACTED]");
        assert_eq!(result["user"]["bio"], "Reach me at [REDACTED]");
        assert_eq!(result["notes"], "Contact [REDACTED]");
    }

    #[test]
    fn empty_policy_is_noop() {
        let r = PiiRedactor::from_policy(&DataPolicyIr::default());
        assert!(!r.is_active());
        let input = serde_json::json!({"email": "test@example.com"});
        assert_eq!(r.redact_json(&input), input);
    }
}
