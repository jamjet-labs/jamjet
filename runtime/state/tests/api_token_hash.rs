//! `api_tokens.token_hash` is stored, so its format has to survive upgrades
//! of the hashing crates: a token issued before one must still validate
//! after it.
//!
//! `TOKEN_SHA256` was computed outside Rust
//! (`printf %s "$TOKEN" | shasum -a 256`), so it does not depend on the
//! `sha2` version under test.

use chrono::Utc;
use jamjet_state::backend::StateBackend;
use jamjet_state::tenant::TenantId;
use jamjet_state::{sha256_hex, SqliteBackend};

const TOKEN: &str = "jj_000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const TOKEN_SHA256: &str = "d67498acd2ad860d04a526dfc08580c4c0beeadcb2a8200091da1fe2acf56a9f";

async fn open_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("open in-memory SQLite")
}

#[test]
fn the_token_hash_is_the_known_answer() {
    assert_eq!(sha256_hex(TOKEN.as_bytes()), TOKEN_SHA256);
}

/// A row as an earlier build wrote it validates on both SQLite backends.
#[tokio::test]
async fn a_token_stored_by_an_earlier_build_still_validates() {
    let db = open_db().await;
    sqlx::query(
        "INSERT INTO api_tokens (id, token_hash, name, role, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind("tok-old")
    .bind(TOKEN_SHA256)
    .bind("issued before the upgrade")
    .bind("operator")
    .bind(Utc::now().to_rfc3339())
    .execute(&db.pool())
    .await
    .unwrap();

    let found = db.validate_token(TOKEN).await.unwrap();
    assert_eq!(found.map(|t| t.id).as_deref(), Some("tok-old"));
    let scoped = db.for_tenant(TenantId::default());
    let found = scoped.validate_token(TOKEN).await.unwrap();
    assert_eq!(found.map(|t| t.id).as_deref(), Some("tok-old"));

    // Control: the lookup is by hash, so another token finds nothing.
    let other = TOKEN.replace("1f", "1e");
    assert!(db.validate_token(&other).await.unwrap().is_none());
}

/// What a new token stores is the same function of its text.
#[tokio::test]
async fn a_new_token_stores_the_hex_sha256_of_its_text() {
    let db = open_db().await;
    let scoped = db.for_tenant(TenantId::default());
    for (plain, info) in [
        db.create_token("unscoped", "operator").await.unwrap(),
        scoped.create_token("scoped", "operator").await.unwrap(),
    ] {
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM api_tokens WHERE id = ?")
            .bind(&info.id)
            .fetch_one(&db.pool())
            .await
            .unwrap();
        assert_eq!(stored, sha256_hex(plain.as_bytes()), "{plain}");
        assert!(db.validate_token(&plain).await.unwrap().is_some());
    }
}
