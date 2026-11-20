//! `api_tokens.token_hash` is stored, so its format has to survive upgrades
//! of the hashing crates: a token issued before one must still validate
//! after it.
//!
//! `TOKEN_SHA256` was computed outside Rust
//! (`printf %s "$TOKEN" | shasum -a 256`), so it does not depend on the
//! `sha2` version under test.

use crate::conformance::cases::support::not_on;
use crate::conformance::{Fixture, StoreKind};
use jamjet_state::sha256_hex;

const TOKEN: &str = "jj_000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const TOKEN_SHA256: &str = "d67498acd2ad860d04a526dfc08580c4c0beeadcb2a8200091da1fe2acf56a9f";

const NO_HASH: &str = "memory keys tokens by their plaintext and stores no hash";

/// A row as an earlier build wrote it validates through the fixture's
/// backend and through the unscoped one.
pub async fn a_token_stored_by_an_earlier_build_still_validates(fx: &Fixture) {
    if not_on(fx, &[StoreKind::Memory], NO_HASH) {
        return;
    }
    assert!(
        fx.hooks
            .insert_token_row(
                "tok-old",
                TOKEN_SHA256,
                "issued before the upgrade",
                "operator"
            )
            .await
    );
    let unscoped = fx.hooks.unscoped();
    for db in [fx.backend.as_ref(), unscoped.as_ref()] {
        let found = db.validate_token(TOKEN).await.unwrap();
        assert_eq!(
            found.map(|t| t.id).as_deref(),
            Some("tok-old"),
            "{:?}",
            fx.kind
        );
        // Control: the lookup is by hash, so another token finds nothing.
        let other = TOKEN.replace("1f", "1e");
        assert!(db.validate_token(&other).await.unwrap().is_none());
    }
}

/// What a new token stores is the same function of its text.
pub async fn a_new_token_stores_the_hex_sha256_of_its_text(fx: &Fixture) {
    if not_on(fx, &[StoreKind::Memory], NO_HASH) {
        return;
    }
    let unscoped = fx.hooks.unscoped();
    for (plain, info) in [
        fx.backend
            .create_token("from-backend", "operator")
            .await
            .unwrap(),
        unscoped.create_token("unscoped", "operator").await.unwrap(),
    ] {
        let stored = fx
            .hooks
            .stored_token_hash(&info.id)
            .await
            .expect("the token's row");
        assert_eq!(stored, sha256_hex(plain.as_bytes()), "{plain}");
        assert!(fx.backend.validate_token(&plain).await.unwrap().is_some());
    }
}
