//! Tamper-evidence tests for the signed, hash-chained audit log.
//!
//! Covers the chain invariants directly on `AuditLogEntry`/`verify_chain`, the
//! adversarial duals (tampered content, forged signature, wrong key, broken
//! downstream link), and the `AuditEnricher` write path (every appended entry
//! is sealed into a verifiable chain).

use std::sync::Arc;

use async_trait::async_trait;
use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery,
    AuditSigner, ChainError,
};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::event::{Event, EventKind};
use uuid::Uuid;

fn make_entry(seq: i64, actor: &str) -> AuditLogEntry {
    AuditLogEntry::new(
        Uuid::new_v4(),
        "exec-1".to_string(),
        seq,
        "node_completed".to_string(),
        actor.to_string(),
        ActorType::System,
        serde_json::json!({ "type": "node_completed", "seq": seq }),
    )
}

/// Seal a slice of entries into a single chain (genesis prev_hash = None).
fn seal_chain(entries: &mut [AuditLogEntry], signer: &AuditSigner) {
    let mut prev: Option<String> = None;
    for entry in entries.iter_mut() {
        entry.seal(prev.clone(), signer);
        prev = entry.entry_hash.clone();
    }
}

#[test]
fn three_entry_chain_verifies() {
    let signer = AuditSigner::new(b"test-key".to_vec());
    let mut entries = vec![make_entry(0, "a"), make_entry(1, "b"), make_entry(2, "c")];
    seal_chain(&mut entries, &signer);

    assert!(verify_chain(&entries, &signer).is_ok());

    // Genesis has no predecessor; each later entry links to the one before it.
    assert!(entries[0].prev_hash.is_none());
    assert_eq!(entries[1].prev_hash, entries[0].entry_hash);
    assert_eq!(entries[2].prev_hash, entries[1].entry_hash);
    // Every entry is sealed (hash + signature present).
    assert!(entries
        .iter()
        .all(|e| e.entry_hash.is_some() && e.signature.is_some()));
}

#[test]
fn tampering_entry_content_breaks_verification_from_that_point() {
    let signer = AuditSigner::new(b"test-key".to_vec());
    let mut entries = vec![make_entry(0, "a"), make_entry(1, "b"), make_entry(2, "c")];
    seal_chain(&mut entries, &signer);

    // Mutate entry 2's (index 1) content but leave its hash + signature intact —
    // exactly what an attacker editing the stored row would do.
    entries[1].actor_id = "tampered".to_string();

    let err = verify_chain(&entries, &signer).unwrap_err();
    assert_eq!(err, ChainError::HashMismatch { index: 1 });

    // The untampered prefix (entry 1 alone) still verifies; the break is at 2..n.
    assert!(verify_chain(&entries[..1], &signer).is_ok());
}

#[test]
fn forged_signature_fails() {
    let signer = AuditSigner::new(b"test-key".to_vec());
    let mut entries = vec![make_entry(0, "a"), make_entry(1, "b"), make_entry(2, "c")];
    seal_chain(&mut entries, &signer);

    // Swap in a well-formed but wrong signature (64 hex chars).
    entries[1].signature = Some("ab".repeat(32));

    let err = verify_chain(&entries, &signer).unwrap_err();
    assert_eq!(err, ChainError::BadSignature { index: 1 });
}

#[test]
fn rehashed_tamper_without_the_key_fails_on_signature() {
    let signer = AuditSigner::new(b"real-key".to_vec());
    let mut entries = vec![make_entry(0, "a"), make_entry(1, "b"), make_entry(2, "c")];
    seal_chain(&mut entries, &signer);

    // Attacker tampers content and recomputes the hash so the integrity check
    // would pass — but can only sign with their OWN key.
    let attacker = AuditSigner::new(b"attacker-key".to_vec());
    let prev = entries[0].entry_hash.clone();
    entries[1].actor_id = "tampered".to_string();
    entries[1].seal(prev, &attacker);

    let err = verify_chain(&entries, &signer).unwrap_err();
    assert_eq!(err, ChainError::BadSignature { index: 1 });
}

#[test]
fn relinking_a_single_entry_breaks_the_downstream_link() {
    let signer = AuditSigner::new(b"real-key".to_vec());
    let mut entries = vec![make_entry(0, "a"), make_entry(1, "b"), make_entry(2, "c")];
    seal_chain(&mut entries, &signer);

    // Even WITH the key, rewriting entry 2 in place changes its hash; entry 3
    // still points at the original hash, so the chain breaks downstream.
    let prev = entries[0].entry_hash.clone();
    entries[1].actor_id = "rewritten".to_string();
    entries[1].seal(prev, &signer);

    let err = verify_chain(&entries, &signer).unwrap_err();
    assert_eq!(err, ChainError::BrokenLink { index: 2 });
}

#[test]
fn unsealed_entry_is_rejected() {
    let signer = AuditSigner::new(b"k".to_vec());
    let entries = vec![make_entry(0, "a")]; // never sealed
    assert_eq!(
        verify_chain(&entries, &signer).unwrap_err(),
        ChainError::Unsealed { index: 0 }
    );
}

// ── Enricher write-path: appended entries are sealed + chained ──────────────

/// In-memory backend that keeps every appended entry so the test can verify
/// the chain the enricher produced. `query` returns newest-first to match the
/// SqliteAuditBackend ordering the enricher relies on for chain seeding.
#[derive(Default)]
struct CapturingBackend {
    entries: tokio::sync::Mutex<Vec<AuditLogEntry>>,
}

#[async_trait]
impl AuditBackend for CapturingBackend {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.entries.lock().await.push(entry);
        Ok(())
    }

    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(self.entries.lock().await.iter().rev().cloned().collect())
    }

    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(self.entries.lock().await.len() as u64)
    }
}

fn workflow_event(exec: ExecutionId, seq: i64) -> Event {
    Event::new(
        exec,
        seq,
        EventKind::WorkflowCompleted {
            final_state: serde_json::json!({ "seq": seq }),
        },
    )
}

#[tokio::test]
async fn enricher_seals_and_chains_appended_entries() {
    let backend = Arc::new(CapturingBackend::default());
    let signer = AuditSigner::new(b"enricher-key".to_vec());
    let enricher = AuditEnricher::with_signer(backend.clone(), signer.clone());

    let exec = ExecutionId(Uuid::new_v4());
    for seq in 0..3 {
        enricher
            .enrich_and_append(&workflow_event(exec.clone(), seq), None)
            .await;
    }

    let captured = backend.entries.lock().await.clone();
    assert_eq!(captured.len(), 3);
    // The whole written chain verifies under the same signer.
    verify_chain(&captured, &signer).expect("enricher must produce a verifiable chain");
    // And it is a real chain: genesis None, then linked.
    assert!(captured[0].prev_hash.is_none());
    assert_eq!(captured[1].prev_hash, captured[0].entry_hash);
    assert_eq!(captured[2].prev_hash, captured[1].entry_hash);
}
