//! `api_tokens.token_hash` is stored, so its format has to survive upgrades
//! of the hashing crates: a token issued before one must still validate
//! after it.
//!
//! `TOKEN_SHA256` was computed outside Rust
//! (`printf %s "$TOKEN" | shasum -a 256`), so it does not depend on the
//! `sha2` version under test.
//!
//! The cases that store and validate tokens are in the backend suite
//! (`runtime/testkit`, module `api_token_hash`).

use jamjet_state::sha256_hex;

const TOKEN: &str = "jj_000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const TOKEN_SHA256: &str = "d67498acd2ad860d04a526dfc08580c4c0beeadcb2a8200091da1fe2acf56a9f";

#[test]
fn the_token_hash_is_the_known_answer() {
    assert_eq!(sha256_hex(TOKEN.as_bytes()), TOKEN_SHA256);
}
