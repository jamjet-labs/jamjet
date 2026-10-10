//! Ed25519 audit signing: a third party holding only the public key verifies
//! the log offline, and cannot forge it. HMAC mode is unchanged.

use chrono::{TimeZone, Utc};
use jamjet_audit::signer::{
    ed25519_statement, ALLOW_INSECURE_KEY_ENV, ED25519_KEY_ENV, ED25519_KEY_FILE_ENV,
    SIGNING_KEY_ENV, SIGNING_MODE_ENV,
};
use jamjet_audit::{verify_chain, verify_entry, ActorType, AuditLogEntry, AuditSigner, ChainError};
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A seed nobody would type, so finding it in output means it leaked.
const SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const OTHER_SEED_HEX: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

/// Env mutation is process-wide; tests in this binary run in parallel.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV_VARS: [&str; 5] = [
    SIGNING_MODE_ENV,
    ED25519_KEY_ENV,
    ED25519_KEY_FILE_ENV,
    SIGNING_KEY_ENV,
    ALLOW_INSECURE_KEY_ENV,
];

fn signer() -> AuditSigner {
    AuditSigner::ed25519_from_hex(SEED_HEX).unwrap()
}

fn verifier_for(s: &AuditSigner) -> AuditSigner {
    AuditSigner::ed25519_verifier(&s.public_key().unwrap()).unwrap()
}

/// A deterministic entry: fixed ids and a timestamp with nanoseconds, and an
/// event with the JSON shapes a re-encoding can get wrong.
fn entry(n: u8) -> AuditLogEntry {
    let mut e = AuditLogEntry::new(
        Uuid::from_bytes([n; 16]),
        "exec_00000000-0000-0000-0000-000000000001".into(),
        i64::from(n),
        "tool_approval_required".into(),
        "token:7".into(),
        ActorType::System,
        json!({
            "type": "tool_approval_required",
            "node_id": "n1",
            "zeta": 1,
            "alpha": { "b": [1, 2.5, -0.000000125, 1e16, 12345678901234567890u64], "a": null },
            "text": "café ✓ \"quoted\" back\\slash \u{1} tab\t newline\n",
            "cost": 0.1,
            "ok": true
        }),
    );
    e.id = Uuid::from_bytes([0xA0 + n; 16]);
    e.created_at = Utc
        .timestamp_opt(1_791_000_000 + i64::from(n), 123_456_789)
        .unwrap();
    e.tenant_id = "default".into();
    if n == 2 {
        e.tool_call_hash = Some("abc".into());
        e.policy_decision = Some("require_approval".into());
        e.expires_at = Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap());
        e.redacted = true;
    }
    e
}

fn chain(s: &AuditSigner) -> Vec<AuditLogEntry> {
    let mut prev = None;
    (1..=3)
        .map(|n| {
            let mut e = entry(n);
            e.seal(prev.clone(), s);
            prev = e.entry_hash.clone();
            e
        })
        .collect()
}

#[test]
fn the_statement_is_jcs() {
    assert_eq!(
        ed25519_statement("ab12", "cd34"),
        br#"{"alg":"Ed25519","entry_hash":"ab12","kid":"cd34","spec":"jamjet.audit.entry/v1"}"#
    );
}

#[test]
fn a_chain_verifies_with_only_the_public_key() {
    let s = signer();
    let entries = chain(&s);
    assert!(entries[0]
        .signature
        .as_deref()
        .unwrap()
        .starts_with(&format!("ed25519:{}:", s.kid().unwrap())));
    let v = verifier_for(&s);
    assert!(!v.can_sign(), "a verifier must not be able to sign");
    assert_eq!(v.sign(b"x"), None);
    assert_eq!(verify_chain(&entries, &v), Ok(()));
    for e in &entries {
        assert_eq!(verify_entry(e, &v), Ok(()));
    }
}

#[test]
fn a_tampered_entry_fails() {
    let s = signer();
    let v = verifier_for(&s);
    let mut entries = chain(&s);
    entries[1].raw_event["node_id"] = json!("n2");
    assert_eq!(
        verify_chain(&entries, &v),
        Err(ChainError::HashMismatch { index: 1 })
    );
    assert_eq!(
        verify_entry(&entries[1], &v),
        Err(ChainError::HashMismatch { index: 0 })
    );
    // Rehashing the tampered content does not help without the private key.
    let rehashed = entries[1].content_hash(entries[1].prev_hash.as_deref());
    entries[1].entry_hash = Some(rehashed);
    assert_eq!(
        verify_entry(&entries[1], &v),
        Err(ChainError::BadSignature { index: 0 })
    );
}

#[test]
fn a_wrong_key_fails() {
    let entries = chain(&signer());
    let other = AuditSigner::ed25519_from_hex(OTHER_SEED_HEX).unwrap();
    assert_eq!(
        verify_chain(&entries, &verifier_for(&other)),
        Err(ChainError::BadSignature { index: 0 })
    );
}

/// A signature by another key, relabelled with this key's id, still fails.
#[test]
fn a_relabelled_signature_fails() {
    let s = signer();
    let other = AuditSigner::ed25519_from_hex(OTHER_SEED_HEX).unwrap();
    let mut e = entry(1);
    e.seal(None, &other);
    let sig = e.signature.unwrap();
    let hex = sig.rsplit(':').next().unwrap();
    e.signature = Some(format!("ed25519:{}:{hex}", s.kid().unwrap()));
    assert_eq!(
        verify_entry(&e, &verifier_for(&s)),
        Err(ChainError::BadSignature { index: 0 })
    );
}

/// Neither mode accepts the other's signature.
#[test]
fn hmac_and_ed25519_signatures_do_not_cross() {
    let hmac = AuditSigner::new(b"shared-secret".to_vec());
    let ed = signer();
    assert!(verify_chain(&chain(&hmac), &verifier_for(&ed)).is_err());
    assert!(verify_chain(&chain(&ed), &hmac).is_err());
    // Not even a bare hex tag that happens to be 64 bytes long.
    let hash = chain(&ed)[0].entry_hash.clone().unwrap();
    let raw = ed.sign(hash.as_bytes()).unwrap();
    let bare = raw.rsplit(':').next().unwrap();
    assert!(!ed.verify(hash.as_bytes(), bare));
}

/// HMAC mode is byte-for-byte what it was: a known-answer test.
#[test]
fn hmac_mode_is_unchanged() {
    let hmac = AuditSigner::new(b"jamjet-kat-key".to_vec());
    assert_eq!(hmac.mode(), "hmac");
    assert_eq!(
        hmac.sign(b"0123456789abcdef").as_deref(),
        Some("3ea1433f028cd70bfa3e52f19aefc92211661429ea7efaca39b9d2481ac3a753")
    );
    assert_eq!(verify_chain(&chain(&hmac), &hmac), Ok(()));
    assert_eq!(hmac.public_key(), None);
    assert_eq!(hmac.public_jwk(), None);
}

#[test]
fn a_small_order_public_key_is_refused() {
    // The identity point, and a key of the wrong length.
    let mut identity = [0u8; 32];
    identity[0] = 1;
    assert!(AuditSigner::ed25519_verifier(&identity).is_err());
    assert!(AuditSigner::ed25519_verifier(&[7u8; 31]).is_err());
}

#[test]
fn a_non_canonical_signature_is_refused() {
    let s = signer();
    let e = &chain(&s)[0];
    let sig = e.signature.clone().unwrap();
    let (prefix, hex_sig) = sig.rsplit_once(':').unwrap();
    let mut raw = hex::decode(hex_sig).unwrap();
    // S + L (the group order): the same point equation, a non-canonical S.
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let mut carry = 0u16;
    for i in 0..32 {
        let sum = u16::from(raw[32 + i]) + u16::from(L[i]) + carry;
        raw[32 + i] = sum as u8;
        carry = sum >> 8;
    }
    let forged = format!("{prefix}:{}", hex::encode(raw));
    assert!(!verifier_for(&s).verify(e.entry_hash.as_deref().unwrap().as_bytes(), &forged));
}

// ── Key handling ──────────────────────────────────────────────────────────

/// Collects everything logged while `f` runs.
fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf::default();
    let out = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || buf.clone())
        .finish();
    let value = tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    (value, text)
}

fn with_env<T>(vars: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Safety: serialized by ENV_LOCK; restored before unlock.
    unsafe {
        for k in ENV_VARS {
            std::env::remove_var(k);
        }
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
    }
    let out = f();
    unsafe {
        for k in ENV_VARS {
            std::env::remove_var(k);
        }
    }
    out
}

#[test]
fn ed25519_mode_from_env_signs_and_never_logs_the_key() {
    let (s, log) = with_env(
        &[(SIGNING_MODE_ENV, "ed25519"), (ED25519_KEY_ENV, SEED_HEX)],
        || logged(AuditSigner::from_env),
    );
    assert_eq!(s.mode(), "ed25519");
    assert!(s.can_sign());
    assert_eq!(s.kid(), signer().kid());
    assert!(log.contains(s.kid().unwrap()), "the kid is logged: {log}");
    assert!(!log.contains(SEED_HEX), "the seed was logged");
    let debug = format!("{s:?}");
    assert!(!debug.contains(SEED_HEX), "Debug printed the seed: {debug}");
    let jwk = s.public_jwk().unwrap().to_string();
    assert!(!jwk.contains(SEED_HEX));
}

#[test]
fn the_key_can_come_from_a_file() {
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_audit_key_{}", Uuid::new_v4().simple()));
    std::fs::write(&path, format!("{SEED_HEX}\n")).unwrap();
    let s = with_env(
        &[
            (SIGNING_MODE_ENV, "Ed25519"),
            (ED25519_KEY_FILE_ENV, path.to_str().unwrap()),
        ],
        AuditSigner::from_env,
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(s.kid(), signer().kid());
}

/// A bad key leaves the log unsigned. It never falls back to the HMAC key,
/// and neither the warning nor the error carries the key.
#[test]
fn a_bad_key_is_unsigned_not_hmac_and_not_logged() {
    let almost = &SEED_HEX[..63];
    let (s, log) = with_env(
        &[
            (SIGNING_MODE_ENV, "ed25519"),
            (ED25519_KEY_ENV, almost),
            (SIGNING_KEY_ENV, "an-hmac-secret"),
        ],
        || logged(AuditSigner::from_env),
    );
    assert_eq!(s.mode(), "unsigned");
    assert!(!s.can_sign());
    assert!(log.contains("UNSIGNED"), "{log}");
    assert!(!log.contains(almost), "the bad key was logged: {log}");
    assert!(!log.contains("an-hmac-secret"));
    let err = AuditSigner::ed25519_from_hex(almost)
        .unwrap_err()
        .to_string();
    assert!(!err.contains(almost), "{err}");
    let not_hex = "zz".repeat(32);
    let err = AuditSigner::ed25519_from_hex(&not_hex)
        .unwrap_err()
        .to_string();
    assert!(!err.contains(&not_hex), "{err}");
}

#[test]
fn two_key_sources_or_none_are_unsigned() {
    let both = with_env(
        &[
            (SIGNING_MODE_ENV, "ed25519"),
            (ED25519_KEY_ENV, SEED_HEX),
            (ED25519_KEY_FILE_ENV, "/nonexistent"),
        ],
        AuditSigner::from_env,
    );
    assert_eq!(both.mode(), "unsigned");
    let none = with_env(&[(SIGNING_MODE_ENV, "ed25519")], AuditSigner::from_env);
    assert_eq!(none.mode(), "unsigned");
    let unknown = with_env(
        &[(SIGNING_MODE_ENV, "rsa"), (SIGNING_KEY_ENV, "k")],
        AuditSigner::from_env,
    );
    assert_eq!(unknown.mode(), "unsigned");
}

/// No mode set: HMAC, as before, even with an Ed25519 key configured.
#[test]
fn the_default_mode_is_hmac() {
    let s = with_env(
        &[(SIGNING_KEY_ENV, "k"), (ED25519_KEY_ENV, SEED_HEX)],
        AuditSigner::from_env,
    );
    assert_eq!(s.mode(), "hmac");
}

// ── Cross-language vector ─────────────────────────────────────────────────

/// The Python verifier (`jamjet.runtime_audit`) is tested against this chain,
/// written as `GET /audit` serializes it. Regenerate with
/// `JAMJET_REGEN_AUDIT_VECTOR=1`; otherwise the checked-in copy must match.
#[test]
fn the_cross_language_vector_is_current() {
    let s = signer();
    let entries = chain(&s);
    let vector = json!({
        "public_key_hex": hex::encode(s.public_key().unwrap()),
        "kid": s.kid(),
        "audit": { "items": serde_json::to_value(&entries).unwrap() },
    });
    let text = serde_json::to_string_pretty(&vector).unwrap() + "\n";
    assert!(!text.contains(SEED_HEX));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/python/tests/fixtures/runtime_audit_ed25519.json");
    if std::env::var_os("JAMJET_REGEN_AUDIT_VECTOR").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).expect("vector present; regenerate it");
    assert_eq!(
        on_disk, text,
        "the Python test vector is stale; regenerate it"
    );
}

/// The stored form must be exactly `ed25519:<this kid>:<hex>`: a valid
/// signature under another label, or with no prefix, is refused.
#[test]
fn the_signature_label_is_checked() {
    let s = signer();
    let e = &chain(&s)[0];
    let hash = e.entry_hash.as_deref().unwrap().as_bytes();
    let sig = e.signature.clone().unwrap();
    let hex_sig = sig.rsplit(':').next().unwrap();
    let v = verifier_for(&s);
    assert!(v.verify(hash, &sig), "control: the stored form verifies");
    assert!(!v.verify(hash, &format!("ed25519:0000000000000000:{hex_sig}")));
    assert!(!v.verify(hash, &format!("{}:{hex_sig}", s.kid().unwrap())));
    assert!(!v.verify(hash, &format!("ED25519:{}:{hex_sig}", s.kid().unwrap())));
}

/// A signature whose R is a small-order point verifies under the plain
/// Ed25519 equation when made with the private key (S = k*a, R = identity),
/// but proves nothing about R. Strict verification refuses it. Made with
/// `SEED_HEX` over the statement for entry hash "aa" * 32 (review T1).
#[test]
fn a_small_order_r_is_refused() {
    let s = signer();
    let sig = format!(
        "ed25519:{}:{}",
        s.kid().unwrap(),
        "0100000000000000000000000000000000000000000000000000000000000000\
         38e5e01276466c089c0f2f58e5d5953165d544e2984f7d026cbd3cf0704d9102"
    );
    let hash = "aa".repeat(32);
    assert!(!verifier_for(&s).verify(hash.as_bytes(), &sig));
    assert!(!s.verify(hash.as_bytes(), &sig));
}

/// A key file others can read is used, with a warning (review M4).
#[cfg(unix)]
#[test]
fn a_key_file_others_can_read_is_warned_about() {
    use std::os::unix::fs::PermissionsExt;
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_audit_key_{}", Uuid::new_v4().simple()));
    std::fs::write(&path, SEED_HEX).unwrap();
    let check = |mode: u32| {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        with_env(
            &[
                (SIGNING_MODE_ENV, "ed25519"),
                (ED25519_KEY_FILE_ENV, path.to_str().unwrap()),
            ],
            || logged(AuditSigner::from_env),
        )
    };
    let (s, log) = check(0o644);
    assert_eq!(s.mode(), "ed25519");
    assert!(log.contains("readable by other users"), "{log}");
    let (_, log) = check(0o600);
    assert!(!log.contains("readable by other users"), "{log}");
    std::fs::remove_file(&path).unwrap();
}
