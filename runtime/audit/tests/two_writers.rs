//! Two processes append to one audit log: the server and `jamjet-server token
//! create` run against the same database, each with its own enricher and its
//! own idea of the chain's head. The chain must stay one line.

use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditQuery, AuditRecord, AuditSigner,
    SqliteAuditBackend,
};
use std::sync::Arc;

fn record(kind: &str) -> AuditRecord {
    AuditRecord {
        event_type: kind.into(),
        actor_id: "cli".into(),
        actor_type: ActorType::Human,
        tenant_id: "default".into(),
        detail: serde_json::json!({ "type": kind }),
    }
}

async fn entries(backend: &SqliteAuditBackend) -> Vec<jamjet_audit::AuditLogEntry> {
    let mut all = backend
        .query(&AuditQuery {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    all.reverse();
    all
}

/// The server writes, the CLI writes, the server writes again. The server's
/// head is stale on its second write; before appends were linked it sealed
/// onto it and forked the chain (`BrokenLink` at the third entry).
#[tokio::test]
async fn a_second_writer_does_not_fork_the_chain() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let reader = SqliteAuditBackend::open(&url).await.unwrap();
    reader.migrate().await.unwrap();
    let signer = AuditSigner::new(b"a-test-key".to_vec());
    let server = AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        signer.clone(),
    );
    let cli = AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        signer.clone(),
    );

    server.seal_record(record("first")).await.unwrap();
    cli.seal_record(record("token_issued")).await.unwrap();
    server.seal_record(record("third")).await.unwrap();
    cli.seal_record(record("token_revoked")).await.unwrap();

    let log = entries(&reader).await;
    let kinds: Vec<&str> = log.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["first", "token_issued", "third", "token_revoked"]);
    verify_chain(&log, &signer).expect("one chain");
    let _ = std::fs::remove_file(path);
}

/// `append_linked` refuses an entry sealed onto a head that is not the log's
/// newest entry, and `head` reads the newest entry's hash.
#[tokio::test]
async fn a_linked_append_onto_a_stale_head_is_refused() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let backend = SqliteAuditBackend::open(&url).await.unwrap();
    backend.migrate().await.unwrap();
    assert_eq!(backend.head().await.unwrap(), None);
    let enricher = AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        AuditSigner::new(b"k".to_vec()),
    );
    enricher.seal_record(record("one")).await.unwrap();
    let head = backend.head().await.unwrap();
    assert!(head.is_some());

    let mut stale = entries(&backend).await.remove(0);
    stale.id = uuid::Uuid::new_v4();
    stale.seal(None, &AuditSigner::new(b"k".to_vec()));
    assert!(
        !backend.append_linked(stale.clone()).await.unwrap(),
        "sealed onto genesis"
    );
    stale.seal(head.clone(), &AuditSigner::new(b"k".to_vec()));
    assert!(backend.append_linked(stale).await.unwrap());
    let _ = std::fs::remove_file(path);
}
