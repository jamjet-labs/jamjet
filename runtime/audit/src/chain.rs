//! Tamper-evidence verification for a sealed audit-log chain.
//!
//! [`verify_chain`] re-walks an ordered slice of entries and, for each one:
//! recomputes its content hash, checks the recomputed hash equals the stored
//! `entry_hash`, checks `prev_hash` links to the previous entry's hash, and
//! checks the signature over `entry_hash` with the signer's key. Any deviation
//! — a mutated field, a re-linked chain, a forged or stripped signature — is
//! reported as an error naming the offending index.

use crate::entry::AuditLogEntry;
use crate::signer::AuditSigner;
use thiserror::Error;

/// Why a chain failed verification. The `index` is the position in the slice
/// passed to [`verify_chain`] (0-based).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("entry {index} has no entry_hash (never sealed)")]
    Unsealed { index: usize },
    #[error("entry {index} has no signature (never sealed)")]
    Unsigned { index: usize },
    #[error("entry {index} prev_hash does not link to the previous entry's hash")]
    BrokenLink { index: usize },
    #[error("entry {index} content hash does not match its recorded entry_hash (tampered)")]
    HashMismatch { index: usize },
    #[error("entry {index} signature is invalid for its entry_hash (forged or wrong key)")]
    BadSignature { index: usize },
}

impl ChainError {
    /// The index of the entry at which verification failed.
    pub fn index(&self) -> usize {
        match self {
            ChainError::Unsealed { index }
            | ChainError::Unsigned { index }
            | ChainError::BrokenLink { index }
            | ChainError::HashMismatch { index }
            | ChainError::BadSignature { index } => *index,
        }
    }
}

/// Verify a contiguous, ordered slice of sealed audit entries.
///
/// `entries` must be in chain order (oldest first); the first entry is treated
/// as the chain segment's genesis (its `prev_hash` must be `None`). Returns
/// `Ok(())` only when every entry's hash, linkage, and signature check out.
pub fn verify_chain(entries: &[AuditLogEntry], signer: &AuditSigner) -> Result<(), ChainError> {
    let mut prev: Option<&str> = None;
    for (index, entry) in entries.iter().enumerate() {
        let entry_hash = entry
            .entry_hash
            .as_deref()
            .ok_or(ChainError::Unsealed { index })?;
        let signature = entry
            .signature
            .as_deref()
            .ok_or(ChainError::Unsigned { index })?;

        // 1. Linkage: this entry's prev_hash must equal the previous entry's
        //    entry_hash (and None at the genesis).
        if entry.prev_hash.as_deref() != prev {
            return Err(ChainError::BrokenLink { index });
        }

        // 2. Integrity: recomputing the content hash over the stored fields +
        //    the stored prev_hash must reproduce the recorded entry_hash. A
        //    tampered field changes the recomputed hash.
        if entry.content_hash(entry.prev_hash.as_deref()) != entry_hash {
            return Err(ChainError::HashMismatch { index });
        }

        // 3. Authenticity: the signature must verify against entry_hash. An
        //    attacker who recomputes the hash to match tampered content still
        //    cannot produce a valid signature without the key.
        if !signer.verify(entry_hash.as_bytes(), signature) {
            return Err(ChainError::BadSignature { index });
        }

        prev = Some(entry_hash);
    }
    Ok(())
}
