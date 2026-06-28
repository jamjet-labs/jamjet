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
//! ## Key source (honest, fail-closed — no silent insecure default)
//!
//! The signing key is read once from the `JAMJET_AUDIT_SIGNING_KEY`
//! environment variable (its raw UTF-8 bytes are the HMAC key). The fallback
//! when it is absent/empty is a **deliberate opt-in**, never silent:
//!
//! * `JAMJET_AUDIT_SIGNING_KEY` set (non-empty) -> sign with it (secure).
//! * else `JAMJET_AUDIT_ALLOW_INSECURE_KEY` set -> sign with the **built-in,
//!   publicly-known dev key**, logging loudly at `warn`. This keeps local
//!   development and tests verifiable behind an explicit, intentional opt-in.
//! * else (no key, no opt-in) -> the signer is **unsigned/refused**: it does
//!   NOT sign (entries are left unsigned and [`crate::verify_chain`] reports
//!   them as `Unsigned`) and [`AuditSigner::verify`] always returns `false`.
//!   It logs loudly at `warn`. The audit log never *pretends* to be securely
//!   signed with a forgeable, publicly-known key.
//!
//! The engine never hard-fails at startup for a missing key (audit may be off);
//! instead the signed-audit path refuses to produce a trustworthy-looking
//! signature unless a real key — or the explicit insecure opt-in — is present.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tracing::warn;

type HmacSha256 = Hmac<Sha256>;

/// The environment variable holding the raw audit signing key.
pub const SIGNING_KEY_ENV: &str = "JAMJET_AUDIT_SIGNING_KEY";

/// Opt-in flag to use the insecure built-in dev key when no real key is set.
pub const ALLOW_INSECURE_KEY_ENV: &str = "JAMJET_AUDIT_ALLOW_INSECURE_KEY";

/// Built-in dev key, used ONLY behind the explicit `JAMJET_AUDIT_ALLOW_INSECURE_KEY`
/// opt-in.
///
/// This value is public by design; signatures produced with it provide NO
/// security. Production deployments MUST set `JAMJET_AUDIT_SIGNING_KEY`.
pub const DEV_DEFAULT_KEY: &str =
    "jamjet-insecure-dev-audit-signing-key-set-JAMJET_AUDIT_SIGNING_KEY-in-prod";

/// Signs and verifies audit entries with a keyed HMAC-SHA256.
///
/// A signer with no key (`key: None`) is the *unsigned/refused* signer: it
/// cannot sign or verify, so it can never masquerade as secure.
#[derive(Clone)]
pub struct AuditSigner {
    key: Option<Vec<u8>>,
    /// True when the built-in insecure dev key is in use.
    is_dev_key: bool,
}

impl AuditSigner {
    /// Build a signer from explicit key bytes (used in tests and by callers
    /// that load the key from their own config/secret store).
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self {
            key: Some(key.into()),
            is_dev_key: false,
        }
    }

    /// Build an *unsigned/refused* signer: no key, so it cannot sign or verify.
    /// Used when no key is configured and the insecure dev key was not opted in.
    pub fn unsigned() -> Self {
        Self {
            key: None,
            is_dev_key: false,
        }
    }

    /// Build a signer from the environment with the honest, fail-closed policy
    /// documented at the module level: real key -> secure; explicit insecure
    /// opt-in -> dev key (loud warn); otherwise -> unsigned (loud warn).
    pub fn from_env() -> Self {
        match std::env::var(SIGNING_KEY_ENV) {
            Ok(key) if !key.is_empty() => Self::new(key.into_bytes()),
            _ => {
                let opted_in = std::env::var(ALLOW_INSECURE_KEY_ENV)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if opted_in {
                    warn!(
                        env = SIGNING_KEY_ENV,
                        opt_in = ALLOW_INSECURE_KEY_ENV,
                        "{SIGNING_KEY_ENV} is not set; signing the audit log with the \
                         BUILT-IN INSECURE DEV KEY because {ALLOW_INSECURE_KEY_ENV} is set. \
                         Audit signatures provide NO tamper-resistance against an attacker. \
                         Provision {SIGNING_KEY_ENV} with a real secret in production."
                    );
                    Self {
                        key: Some(DEV_DEFAULT_KEY.as_bytes().to_vec()),
                        is_dev_key: true,
                    }
                } else {
                    warn!(
                        env = SIGNING_KEY_ENV,
                        opt_in = ALLOW_INSECURE_KEY_ENV,
                        "{SIGNING_KEY_ENV} is not set; audit entries will be hash-chained \
                         but left UNSIGNED (verification reports them as unsigned). Set \
                         {SIGNING_KEY_ENV} to a real secret, or {ALLOW_INSECURE_KEY_ENV}=1 \
                         to explicitly opt into the insecure built-in dev key."
                    );
                    Self::unsigned()
                }
            }
        }
    }

    /// True when this signer holds a key and can therefore sign/verify.
    pub fn can_sign(&self) -> bool {
        self.key.is_some()
    }

    /// True when this signer is using the insecure built-in dev key.
    pub fn is_dev_key(&self) -> bool {
        self.is_dev_key
    }

    /// Sign `data`, returning the lowercase-hex HMAC-SHA256 tag, or `None` when
    /// this signer has no key (the unsigned/refused signer).
    pub fn sign(&self, data: &[u8]) -> Option<String> {
        let key = self.key.as_ref()?;
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
        mac.update(data);
        Some(hex::encode(mac.finalize().into_bytes()))
    }

    /// Verify a hex-encoded signature over `data` in constant time.
    ///
    /// Returns `false` for an unsigned/refused signer (no key), a malformed
    /// (non-hex) signature, or any mismatch.
    pub fn verify(&self, data: &[u8], signature_hex: &str) -> bool {
        let Some(key) = self.key.as_ref() else {
            return false;
        };
        let Ok(expected) = hex::decode(signature_hex) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
        mac.update(data);
        // `verify_slice` is a constant-time comparison (no timing oracle).
        mac.verify_slice(&expected).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes env-var mutation across the (parallel) tests in this module.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sign_then_verify_roundtrips() {
        let signer = AuditSigner::new(b"unit-test-key".to_vec());
        let sig = signer.sign(b"hello").expect("real signer signs");
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
        let sig = signer.sign(b"hello").expect("real signer signs");
        assert!(!signer.verify(b"hell0", &sig));
    }

    #[test]
    fn verify_fails_under_a_different_key() {
        let signer = AuditSigner::new(b"key-a".to_vec());
        let attacker = AuditSigner::new(b"key-b".to_vec());
        let sig = signer.sign(b"payload").expect("real signer signs");
        assert!(!attacker.verify(b"payload", &sig));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        let signer = AuditSigner::new(b"k".to_vec());
        assert!(!signer.verify(b"data", "not-hex-zzzz"));
    }

    #[test]
    fn unsigned_signer_cannot_sign_or_verify() {
        let signer = AuditSigner::unsigned();
        assert!(!signer.can_sign());
        assert!(signer.sign(b"x").is_none(), "unsigned signer must not sign");
        // Even a structurally valid hex signature must not verify under no key.
        let real = AuditSigner::new(b"k".to_vec());
        let sig = real.sign(b"x").expect("real signer signs");
        assert!(
            !signer.verify(b"x", &sig),
            "unsigned signer must never verify-as-signed"
        );
    }

    #[test]
    fn from_env_uses_real_key_when_set() {
        let _g = ENV_LOCK.lock().unwrap();
        // Safety: guarded by ENV_LOCK; cleaned up before unlock.
        unsafe {
            std::env::set_var(SIGNING_KEY_ENV, "a-real-secret");
            std::env::remove_var(ALLOW_INSECURE_KEY_ENV);
        }
        let signer = AuditSigner::from_env();
        unsafe {
            std::env::remove_var(SIGNING_KEY_ENV);
        }
        assert!(signer.can_sign());
        assert!(!signer.is_dev_key());
    }

    #[test]
    fn from_env_refuses_without_key_or_opt_in() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(SIGNING_KEY_ENV);
            std::env::remove_var(ALLOW_INSECURE_KEY_ENV);
        }
        let signer = AuditSigner::from_env();
        assert!(
            !signer.can_sign(),
            "no key + no opt-in must be unsigned/refused, not the dev key"
        );
        assert!(signer.sign(b"x").is_none());
    }

    #[test]
    fn from_env_uses_dev_key_only_with_explicit_opt_in() {
        let _g = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var(SIGNING_KEY_ENV);
            std::env::set_var(ALLOW_INSECURE_KEY_ENV, "1");
        }
        let signer = AuditSigner::from_env();
        unsafe {
            std::env::remove_var(ALLOW_INSECURE_KEY_ENV);
        }
        assert!(signer.can_sign());
        assert!(signer.is_dev_key(), "explicit opt-in selects the dev key");
    }
}
