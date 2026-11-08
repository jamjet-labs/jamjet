//! API token text, and the hash `api_tokens.token_hash` stores for it.
//!
//! Both SQLite backends issue and check tokens through these functions, so
//! the two cannot drift apart. The hash is persisted: a token issued by one
//! build must still validate on the next, which `tests/api_token_hash.rs`
//! pins with a known answer.

use rand::RngExt as _;

/// A new token: `jj_` and 32 random bytes as lowercase hex.
///
/// The bytes come from `rand::rng()`, a ChaCha12 generator seeded and
/// reseeded from the operating system. [`secure`] makes the compiler check
/// that it is a cryptographically secure generator, so a faster one cannot
/// be swapped in unnoticed.
pub(crate) fn generate() -> String {
    let bytes: [u8; 32] = secure(rand::rng()).random();
    format!(
        "jj_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// What `api_tokens.token_hash` holds for `token`: the lowercase hex
/// SHA-256 of its text.
pub(crate) fn hash(token: &str) -> String {
    crate::hashing::sha256_hex(token.as_bytes())
}

fn secure<R: rand::CryptoRng>(rng: R) -> R {
    rng
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_token_is_jj_and_64_hex_digits() {
        let token = super::generate();
        let hex = token.strip_prefix("jj_").expect("jj_ prefix");
        assert_eq!(hex.len(), 64, "{token}");
        assert!(hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_ne!(token, super::generate());
    }
}
