//! `AuditSigner` — keyed HMAC-SHA256 signatures over audit-log entries.
//!
//! The signature authenticates each entry's content hash so that a tampered
//! entry (or a forged one written by an attacker who does not hold the key)
//! fails verification. HMAC-SHA256 is used because it is the strongest signer
//! available from the already-vendored crypto crates (`hmac` + `sha2`); the
//! workspace carries no asymmetric signer (`ed25519-dalek`/`ring` is not a
//! direct dependency). The signature is a symmetric MAC: the same key signs
//! and verifies. Per-tenant keys / rotation / an asymmetric upgrade are a
//! documented follow-up (`F-t3-key-mgmt`).
//!
//! ## Key source
//!
//! The signing key is read once from the `JAMJET_AUDIT_SIGNING_KEY`
//! environment variable (its raw UTF-8 bytes are the HMAC key). When the
//! variable is absent or empty the signer falls back to a **built-in,
//! publicly-known dev key** and logs loudly at `warn`. This keeps local
//! development and tests verifiable (a fixed key round-trips across process
//! restarts, unlike an ephemeral random key) while making it unmistakable
//! that the signatures are NOT secure until a real key is provisioned. The
//! engine never hard-fails for a missing key.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

/// The environment variable holding the raw audit signing key.
pub const SIGNING_KEY_ENV: &str = "JAMJET_AUDIT_SIGNING_KEY";

/// Built-in dev key used when `JAMJET_AUDIT_SIGNING_KEY` is unset.
///
/// This value is public by design; signatures produced with it are
/// reproducible but provide NO security. Production deployments MUST set
/// `JAMJET_AUDIT_SIGNING_KEY`.
pub const DEV_DEFAULT_KEY: &str =
    "jamjet-insecure-dev-audit-signing-key-set-JAMJET_AUDIT_SIGNING_KEY-in-prod";

/// Signs and verifies audit entries with a keyed HMAC-SHA256.
#[derive(Clone)]
pub struct AuditSigner {
    key: Vec<u8>,
    /// True when the built-in insecure dev key is in use.
    is_dev_key: bool,
}

impl AuditSigner {
    /// Build a signer from explicit key bytes (used in tests and by callers
    /// that load the key from their own config/secret store).
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self {
            key: key.into(),
            is_dev_key: false,
        }
    }

    /// Build a signer from `JAMJET_AUDIT_SIGNING_KEY`, falling back to the
    /// built-in dev key (with a loud `warn`) when the variable is unset/empty.
    pub fn from_env() -> Self {
        match std::env::var(SIGNING_KEY_ENV) {
            Ok(key) if !key.is_empty() => Self::new(key.into_bytes()),
            _ => {
                warn!(
                    env = SIGNING_KEY_ENV,
                    "{SIGNING_KEY_ENV} is not set; signing the audit log with the BUILT-IN \
                     INSECURE DEV KEY. Audit signatures provide NO tamper-resistance against \
                     an attacker until {SIGNING_KEY_ENV} is provisioned with a real secret."
                );
                Self {
                    key: DEV_DEFAULT_KEY.as_bytes().to_vec(),
                    is_dev_key: true,
                }
            }
        }
    }

    /// True when this signer is using the insecure built-in dev key.
    pub fn is_dev_key(&self) -> bool {
        self.is_dev_key
    }

    /// Sign `data`, returning the lowercase-hex HMAC-SHA256 tag.
    pub fn sign(&self, data: &[u8]) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC accepts a key of any length");
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Verify a hex-encoded signature over `data` in constant time.
    ///
    /// Returns `false` for a malformed (non-hex) signature or any mismatch.
    pub fn verify(&self, data: &[u8], signature_hex: &str) -> bool {
        let Ok(expected) = hex::decode(signature_hex) else {
            return false;
        };
        let mut mac =
            HmacSha256::new_from_slice(&self.key).expect("HMAC accepts a key of any length");
        mac.update(data);
        // `verify_slice` is a constant-time comparison (no timing oracle).
        mac.verify_slice(&expected).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrips() {
        let signer = AuditSigner::new(b"unit-test-key".to_vec());
        let sig = signer.sign(b"hello");
        assert!(signer.verify(b"hello", &sig));
    }

    #[test]
    fn signature_is_deterministic_for_same_key_and_data() {
        let signer = AuditSigner::new(b"k".to_vec());
        assert_eq!(signer.sign(b"abc"), signer.sign(b"abc"));
    }

    #[test]
    fn verify_fails_for_tampered_data() {
        let signer = AuditSigner::new(b"k".to_vec());
        let sig = signer.sign(b"hello");
        assert!(!signer.verify(b"hell0", &sig));
    }

    #[test]
    fn verify_fails_under_a_different_key() {
        let signer = AuditSigner::new(b"key-a".to_vec());
        let attacker = AuditSigner::new(b"key-b".to_vec());
        let sig = signer.sign(b"payload");
        assert!(!attacker.verify(b"payload", &sig));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        let signer = AuditSigner::new(b"k".to_vec());
        assert!(!signer.verify(b"data", "not-hex-zzzz"));
    }
}
