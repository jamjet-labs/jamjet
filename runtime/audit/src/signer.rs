//! `AuditSigner` — signatures over audit-log entries, in one of two modes.
//!
//! The signature authenticates each entry's content hash, so a tampered entry
//! (or one forged by someone without the signing key) fails verification.
//!
//! * **HMAC-SHA256** (the default, unchanged): a symmetric MAC. The same key
//!   signs and verifies, so anyone able to verify can also forge.
//! * **Ed25519** (`JAMJET_AUDIT_SIGNING_MODE=ed25519`): each entry is signed
//!   with a private key only the runtime holds; a third party holding only the
//!   public key verifies offline ([`AuditSigner::ed25519_verifier`], or
//!   `jamjet.runtime_audit` in the Python SDK). The public key is published at
//!   `/.well-known/did.json`.
//!
//! ## What an Ed25519 signature covers
//!
//! Every entry is signed, not a chain head at intervals: the chain spans every
//! tenant, so a tenant's own slice of the log is not contiguous, and a
//! per-entry signature is the only one a reader of that slice can check. The
//! signed bytes are a statement in JCS (RFC 8785) canonical JSON, the encoding
//! AgentBoundary's receipts use:
//!
//! ```text
//! {"alg":"Ed25519","entry_hash":"<hex>","kid":"<kid>","spec":"jamjet.audit.entry/v1"}
//! ```
//!
//! `spec` separates these signatures from any other use of the key, and `alg`
//! and `kid` bind the algorithm and key. Every member is an ASCII string, so
//! the JCS form is the members sorted by name with no whitespace. `kid` is the
//! first 16 hex digits of SHA-256 over the raw 32-byte public key. The stored
//! signature is `ed25519:<kid>:<hex>`, so an HMAC verifier never accepts it and
//! an Ed25519 verifier never accepts a bare HMAC tag.
//!
//! What is not JCS: the `entry_hash` the statement names is the existing
//! binary content hash ([`crate::AuditLogEntry::content_hash`]), unchanged so
//! that chains already written verify as before. A verifier recomputes it
//! from the entry's fields, not from a JCS form of the entry.
//!
//! ## Key source (honest, fail-closed — no silent insecure default)
//!
//! HMAC mode reads `JAMJET_AUDIT_SIGNING_KEY` (raw UTF-8 bytes are the key):
//!
//! * set (non-empty) -> sign with it (secure).
//! * else `JAMJET_AUDIT_ALLOW_INSECURE_KEY` set -> sign with the **built-in,
//!   publicly-known dev key**, logging loudly at `warn`.
//! * else -> the signer is **unsigned/refused**: it does NOT sign (entries are
//!   left unsigned and [`crate::verify_chain`] reports them as `Unsigned`) and
//!   [`AuditSigner::verify`] always returns `false`. It logs loudly at `warn`.
//!
//! Ed25519 mode reads a 32-byte seed as 64 hex digits from
//! `JAMJET_AUDIT_ED25519_KEY`, or from the file named by
//! `JAMJET_AUDIT_ED25519_KEY_FILE` (surrounding whitespace ignored). A missing,
//! malformed or doubly configured key leaves the signer unsigned with a `warn`;
//! it never falls back to HMAC. An unknown mode is unsigned too. The key is
//! never logged, never put in an error, and never printed by `Debug`.
//!
//! The engine never hard-fails at startup for a missing key (audit may be off);
//! instead the signed-audit path refuses to produce a trustworthy-looking
//! signature unless a real key — or the explicit insecure opt-in — is present.

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use zeroize::Zeroize;

type HmacSha256 = Hmac<Sha256>;

/// The environment variable holding the raw audit signing key.
pub const SIGNING_KEY_ENV: &str = "JAMJET_AUDIT_SIGNING_KEY";

/// Opt-in flag to use the insecure built-in dev key when no real key is set.
pub const ALLOW_INSECURE_KEY_ENV: &str = "JAMJET_AUDIT_ALLOW_INSECURE_KEY";

/// Selects the signing mode: `hmac` (default) or `ed25519`.
pub const SIGNING_MODE_ENV: &str = "JAMJET_AUDIT_SIGNING_MODE";

/// The Ed25519 seed, as 64 hex digits.
pub const ED25519_KEY_ENV: &str = "JAMJET_AUDIT_ED25519_KEY";

/// A file holding the Ed25519 seed, as 64 hex digits.
pub const ED25519_KEY_FILE_ENV: &str = "JAMJET_AUDIT_ED25519_KEY_FILE";

/// The `spec` member of the statement an Ed25519 entry signature covers.
pub const ED25519_SIG_SPEC: &str = "jamjet.audit.entry/v1";

/// The prefix of a stored Ed25519 entry signature: `ed25519:<kid>:<hex>`.
pub const ED25519_SIG_PREFIX: &str = "ed25519:";

/// Built-in dev key, used ONLY behind the explicit `JAMJET_AUDIT_ALLOW_INSECURE_KEY`
/// opt-in.
///
/// This value is public by design; signatures produced with it provide NO
/// security. Production deployments MUST set `JAMJET_AUDIT_SIGNING_KEY`.
pub const DEV_DEFAULT_KEY: &str =
    "jamjet-insecure-dev-audit-signing-key-set-JAMJET_AUDIT_SIGNING_KEY-in-prod";

/// Why an Ed25519 key could not be used. Never carries the key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("{0} is set but is not 64 hex digits (a 32-byte Ed25519 seed)")]
    MalformedSeed(&'static str),
    #[error("both {ED25519_KEY_ENV} and {ED25519_KEY_FILE_ENV} are set; set one")]
    Ambiguous,
    #[error("neither {ED25519_KEY_ENV} nor {ED25519_KEY_FILE_ENV} is set")]
    Missing,
    #[error("cannot read the key file named by {ED25519_KEY_FILE_ENV}: {0}")]
    Unreadable(String),
    #[error("not a usable Ed25519 public key (32 bytes, not of small order)")]
    BadPublicKey,
}

#[derive(Clone)]
enum Key {
    None,
    Hmac(Vec<u8>),
    Ed25519Signing { key: SigningKey, kid: String },
    Ed25519Verifying { key: VerifyingKey, kid: String },
}

/// Signs and verifies audit entries (see the module docs for the modes).
///
/// A signer with no key is the *unsigned/refused* signer: it cannot sign or
/// verify, so it can never masquerade as secure. An Ed25519 verifier built from
/// a public key verifies but cannot sign.
#[derive(Clone)]
pub struct AuditSigner {
    key: Key,
    /// True when the built-in insecure dev key is in use.
    is_dev_key: bool,
}

/// Shows the mode and, for Ed25519, the public key id. Never key material.
impl std::fmt::Debug for AuditSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditSigner")
            .field("mode", &self.mode())
            .field("kid", &self.kid())
            .field("is_dev_key", &self.is_dev_key)
            .finish()
    }
}

/// The `kid` of a raw Ed25519 public key.
pub fn ed25519_kid(public_key: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(public_key))[..16].to_string()
}

/// The bytes an Ed25519 entry signature covers: the JCS statement naming
/// `entry_hash` and `kid` (see the module docs).
pub fn ed25519_statement(entry_hash: &str, kid: &str) -> Vec<u8> {
    // serde_json writes a `BTreeMap` with its keys sorted and no whitespace,
    // and escapes strings exactly as JCS does, so for these all-string members
    // its output is the JCS form.
    let stmt: std::collections::BTreeMap<&str, &str> = [
        ("alg", "Ed25519"),
        ("entry_hash", entry_hash),
        ("kid", kid),
        ("spec", ED25519_SIG_SPEC),
    ]
    .into_iter()
    .collect();
    serde_json::to_vec(&stmt).expect("a map of strings serializes")
}

/// A 32-byte seed from 64 hex digits. `source` names the variable in the
/// error, never the value.
fn parse_seed(text: &str, source: &'static str) -> Result<[u8; 32], KeyError> {
    let text = text.trim();
    if text.len() != 64 {
        return Err(KeyError::MalformedSeed(source));
    }
    let mut bytes = hex::decode(text).map_err(|_| KeyError::MalformedSeed(source))?;
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(seed)
}

impl AuditSigner {
    /// Build an HMAC signer from explicit key bytes (used in tests and by
    /// callers that load the key from their own config/secret store).
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self {
            key: Key::Hmac(key.into()),
            is_dev_key: false,
        }
    }

    /// Build an *unsigned/refused* signer: no key, so it cannot sign or verify.
    /// Used when no key is configured and the insecure dev key was not opted in.
    pub fn unsigned() -> Self {
        Self {
            key: Key::None,
            is_dev_key: false,
        }
    }

    /// Build an Ed25519 signer from a 32-byte seed.
    pub fn ed25519(seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let kid = ed25519_kid(key.verifying_key().as_bytes());
        Self {
            key: Key::Ed25519Signing { key, kid },
            is_dev_key: false,
        }
    }

    /// Build an Ed25519 signer from a seed written as 64 hex digits.
    pub fn ed25519_from_hex(seed_hex: &str) -> Result<Self, KeyError> {
        let mut seed = parse_seed(seed_hex, ED25519_KEY_ENV)?;
        let signer = Self::ed25519(&seed);
        seed.zeroize();
        Ok(signer)
    }

    /// A verifier holding only a raw 32-byte Ed25519 public key. It verifies
    /// Ed25519 entry signatures and cannot sign. A key of small order is
    /// refused: a signature under one proves nothing.
    pub fn ed25519_verifier(public_key: &[u8]) -> Result<Self, KeyError> {
        let bytes: [u8; 32] = public_key.try_into().map_err(|_| KeyError::BadPublicKey)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| KeyError::BadPublicKey)?;
        if key.is_weak() {
            return Err(KeyError::BadPublicKey);
        }
        Ok(Self {
            key: Key::Ed25519Verifying {
                key,
                kid: ed25519_kid(&bytes),
            },
            is_dev_key: false,
        })
    }

    /// Build a signer from the environment with the honest, fail-closed policy
    /// documented at the module level.
    pub fn from_env() -> Self {
        let mode = std::env::var(SIGNING_MODE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match mode.as_str() {
            "" | "hmac" => {
                if std::env::var_os(ED25519_KEY_ENV).is_some()
                    || std::env::var_os(ED25519_KEY_FILE_ENV).is_some()
                {
                    warn!(
                        "an Ed25519 audit key is configured but {SIGNING_MODE_ENV} is not \
                         `ed25519`; it is ignored and the audit log is signed with HMAC"
                    );
                }
                Self::hmac_from_env()
            }
            "ed25519" => match Self::ed25519_from_env() {
                Ok(signer) => {
                    info!(
                        kid = signer.kid().unwrap_or_default(),
                        "signing the audit log with Ed25519; the public key is served at \
                         /.well-known/did.json"
                    );
                    signer
                }
                Err(e) => {
                    warn!(
                        "{SIGNING_MODE_ENV}=ed25519 but the key cannot be used ({e}); audit \
                         entries will be hash-chained but left UNSIGNED. The HMAC key is \
                         NOT used instead."
                    );
                    Self::unsigned()
                }
            },
            other => {
                warn!(
                    mode = other,
                    "{SIGNING_MODE_ENV} is not `hmac` or `ed25519`; audit entries will be \
                     hash-chained but left UNSIGNED"
                );
                Self::unsigned()
            }
        }
    }

    /// The Ed25519 signer `from_env` builds in `ed25519` mode, or why not.
    fn ed25519_from_env() -> Result<Self, KeyError> {
        let inline = std::env::var(ED25519_KEY_ENV)
            .ok()
            .filter(|v| !v.is_empty());
        let file = std::env::var(ED25519_KEY_FILE_ENV)
            .ok()
            .filter(|v| !v.is_empty());
        let mut seed = match (inline, file) {
            (Some(_), Some(_)) => return Err(KeyError::Ambiguous),
            (None, None) => return Err(KeyError::Missing),
            (Some(mut text), None) => {
                let seed = parse_seed(&text, ED25519_KEY_ENV);
                text.zeroize();
                seed?
            }
            (None, Some(path)) => {
                warn_if_others_can_read(&path);
                let mut text = std::fs::read_to_string(&path)
                    .map_err(|e| KeyError::Unreadable(format!("{path}: {}", e.kind())))?;
                let seed = parse_seed(&text, ED25519_KEY_FILE_ENV);
                text.zeroize();
                seed?
            }
        };
        let signer = Self::ed25519(&seed);
        seed.zeroize();
        Ok(signer)
    }

    /// The HMAC signer `from_env` builds in `hmac` mode.
    fn hmac_from_env() -> Self {
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
                        key: Key::Hmac(DEV_DEFAULT_KEY.as_bytes().to_vec()),
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

    /// `"hmac"`, `"ed25519"`, or `"unsigned"`.
    pub fn mode(&self) -> &'static str {
        match self.key {
            Key::None => "unsigned",
            Key::Hmac(_) => "hmac",
            Key::Ed25519Signing { .. } | Key::Ed25519Verifying { .. } => "ed25519",
        }
    }

    /// True when this signer holds a key it can sign with.
    pub fn can_sign(&self) -> bool {
        matches!(self.key, Key::Hmac(_) | Key::Ed25519Signing { .. })
    }

    /// True when this signer is using the insecure built-in dev key.
    pub fn is_dev_key(&self) -> bool {
        self.is_dev_key
    }

    /// The raw Ed25519 public key, in Ed25519 mode.
    pub fn public_key(&self) -> Option<[u8; 32]> {
        match &self.key {
            Key::Ed25519Signing { key, .. } => Some(key.verifying_key().to_bytes()),
            Key::Ed25519Verifying { key, .. } => Some(key.to_bytes()),
            _ => None,
        }
    }

    /// The Ed25519 key id, in Ed25519 mode.
    pub fn kid(&self) -> Option<&str> {
        match &self.key {
            Key::Ed25519Signing { kid, .. } | Key::Ed25519Verifying { kid, .. } => Some(kid),
            _ => None,
        }
    }

    /// The public key as a JWK (RFC 8037: `kty: OKP`, `crv: Ed25519`), in
    /// Ed25519 mode.
    pub fn public_jwk(&self) -> Option<serde_json::Value> {
        use base64::Engine as _;
        let key = self.public_key()?;
        Some(serde_json::json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
            "kid": self.kid(),
        }))
    }

    /// Sign `data` (an entry hash), or `None` when this signer cannot sign.
    ///
    /// HMAC: the lowercase-hex HMAC-SHA256 tag over `data`. Ed25519:
    /// `ed25519:<kid>:<hex>` over [`ed25519_statement`]; `data` must be UTF-8
    /// (an entry hash is hex), otherwise nothing is signed.
    pub fn sign(&self, data: &[u8]) -> Option<String> {
        match &self.key {
            Key::None | Key::Ed25519Verifying { .. } => None,
            Key::Hmac(key) => {
                let mut mac =
                    HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
                mac.update(data);
                Some(hex::encode(mac.finalize().into_bytes()))
            }
            Key::Ed25519Signing { key, kid } => {
                let entry_hash = std::str::from_utf8(data).ok()?;
                let sig = key.sign(&ed25519_statement(entry_hash, kid));
                Some(format!(
                    "{ED25519_SIG_PREFIX}{kid}:{}",
                    hex::encode(sig.to_bytes())
                ))
            }
        }
    }

    /// Verify `signature` over `data`.
    ///
    /// Returns `false` for an unsigned/refused signer, a malformed signature,
    /// a signature of the other mode, an Ed25519 signature under another key
    /// id, or any mismatch. HMAC is compared in constant time; Ed25519 uses
    /// strict verification (canonical `S`, no small-order keys).
    pub fn verify(&self, data: &[u8], signature: &str) -> bool {
        match &self.key {
            Key::None => false,
            Key::Hmac(key) => {
                let Ok(expected) = hex::decode(signature) else {
                    return false;
                };
                let mut mac =
                    HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
                mac.update(data);
                // `verify_slice` is a constant-time comparison (no timing oracle).
                mac.verify_slice(&expected).is_ok()
            }
            Key::Ed25519Signing { key, kid } => {
                verify_ed25519(&key.verifying_key(), kid, data, signature)
            }
            Key::Ed25519Verifying { key, kid } => verify_ed25519(key, kid, data, signature),
        }
    }
}

/// The key is still used: refusing would leave the log unsigned, which is
/// worse. The path is logged, never the contents.
fn warn_if_others_can_read(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.permissions().mode() & 0o077 != 0 {
                warn!(
                    path,
                    "the Ed25519 audit key file is readable by other users; restrict it \
                     to the runtime's user (chmod 600)"
                );
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn verify_ed25519(key: &VerifyingKey, kid: &str, data: &[u8], signature: &str) -> bool {
    let Some(rest) = signature.strip_prefix(ED25519_SIG_PREFIX) else {
        return false;
    };
    let Some((sig_kid, sig_hex)) = rest.split_once(':') else {
        return false;
    };
    if sig_kid != kid {
        return false;
    }
    let Ok(raw) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(raw) = <[u8; 64]>::try_from(raw.as_slice()) else {
        return false;
    };
    let Ok(entry_hash) = std::str::from_utf8(data) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(&raw);
    key.verify_strict(&ed25519_statement(entry_hash, kid), &sig)
        .is_ok()
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
