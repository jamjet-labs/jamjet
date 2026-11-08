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
    // Dated after the head, so only the link decides.
    stale.created_at += chrono::Duration::microseconds(1);
    stale.seal(None, &AuditSigner::new(b"k".to_vec()));
    assert!(
        !backend.append_linked(stale.clone()).await.unwrap(),
        "sealed onto genesis"
    );
    stale.seal(head.clone(), &AuditSigner::new(b"k".to_vec()));
    assert!(backend.append_linked(stale).await.unwrap());
    let _ = std::fs::remove_file(path);
}

/// An audit backend over SQLite that lets another writer append first, the
/// first time an entry is offered: the CLI landing between this writer's
/// seal and its insert.
struct Raced {
    inner: SqliteAuditBackend,
    other: std::sync::OnceLock<Arc<AuditEnricher>>,
    fired: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl AuditBackend for Raced {
    async fn append(
        &self,
        entry: jamjet_audit::AuditLogEntry,
    ) -> Result<(), jamjet_audit::AuditError> {
        self.inner.append(entry).await
    }
    async fn append_linked(
        &self,
        entry: jamjet_audit::AuditLogEntry,
    ) -> Result<bool, jamjet_audit::AuditError> {
        if !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Let time move on, so the other entry is newer than this one.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if let Some(other) = self.other.get() {
                other.seal_record(record("cut_in")).await.unwrap();
            }
        }
        self.inner.append_linked(entry).await
    }
    async fn head(&self) -> Result<Option<String>, jamjet_audit::AuditError> {
        self.inner.head().await
    }
    async fn head_at(
        &self,
    ) -> Result<Option<(Option<String>, chrono::DateTime<chrono::Utc>)>, jamjet_audit::AuditError>
    {
        self.inner.head_at().await
    }
    async fn query(
        &self,
        q: &AuditQuery,
    ) -> Result<Vec<jamjet_audit::AuditLogEntry>, jamjet_audit::AuditError> {
        self.inner.query(q).await
    }
    async fn count(&self, q: &AuditQuery) -> Result<u64, jamjet_audit::AuditError> {
        self.inner.count(q).await
    }
}

/// An entry that loses the race is sealed again onto the new head with a
/// new `created_at`, so the order entries are listed in (by `created_at`)
/// stays the chain's order.
#[tokio::test]
async fn an_entry_that_loses_the_race_is_sealed_again_after_the_winner() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let reader = SqliteAuditBackend::open(&url).await.unwrap();
    reader.migrate().await.unwrap();
    let signer = AuditSigner::new(b"a-test-key".to_vec());
    let raced = Arc::new(Raced {
        inner: SqliteAuditBackend::open(&url).await.unwrap(),
        other: std::sync::OnceLock::new(),
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    let other = Arc::new(AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        signer.clone(),
    ));
    raced.other.set(other).ok().unwrap();
    let server = AuditEnricher::with_signer(raced, signer.clone());

    server.seal_record(record("slow")).await.unwrap();

    let log = entries(&reader).await;
    let kinds: Vec<&str> = log.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["cut_in", "slow"]);
    verify_chain(&log, &signer).expect("listed in chain order");
    let _ = std::fs::remove_file(path);
}

/// A link dated before the head is refused: the head and the listing order
/// by `created_at`, so an entry linked onto the head but dated before it
/// would list (and read as the head) out of chain order, and the next entry
/// would link onto the old head. A clock that stepped back did that.
#[tokio::test]
async fn a_link_dated_before_the_head_is_refused() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let backend = SqliteAuditBackend::open(&url).await.unwrap();
    backend.migrate().await.unwrap();
    let signer = AuditSigner::new(b"k".to_vec());
    let enricher = AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        signer.clone(),
    );
    enricher.seal_record(record("one")).await.unwrap();
    let head = entries(&backend).await.remove(0);

    let mut back = head.clone();
    back.id = uuid::Uuid::new_v4();
    back.created_at = head.created_at - chrono::Duration::seconds(1);
    back.seal(head.entry_hash.clone(), &signer);
    assert!(
        !backend.append_linked(back.clone()).await.unwrap(),
        "linked onto the head, dated before it"
    );
    back.created_at = head.created_at;
    back.seal(head.entry_hash.clone(), &signer);
    assert!(
        !backend.append_linked(back.clone()).await.unwrap(),
        "dated the same as the head"
    );
    back.created_at = head.created_at + chrono::Duration::microseconds(1);
    back.seal(head.entry_hash.clone(), &signer);
    assert!(backend.append_linked(back).await.unwrap());
    let _ = std::fs::remove_file(path);
}

/// A writer whose clock is behind the head's (a step back, or another
/// machine's clock) dates its entry just after the head, so the chain still
/// lists in order and the head moves to it.
#[tokio::test]
async fn a_clock_behind_the_head_still_extends_the_chain() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let backend = SqliteAuditBackend::open(&url).await.unwrap();
    backend.migrate().await.unwrap();
    let signer = AuditSigner::new(b"k".to_vec());
    // The head, written by a writer an hour ahead.
    let mut ahead = jamjet_audit::AuditLogEntry::new(
        uuid::Uuid::new_v4(),
        String::new(),
        0,
        "ahead".into(),
        "cli".into(),
        ActorType::Human,
        serde_json::json!({ "type": "ahead" }),
    );
    ahead.created_at = chrono::Utc::now() + chrono::Duration::hours(1);
    ahead.seal(None, &signer);
    assert!(backend.append_linked(ahead.clone()).await.unwrap());

    let enricher = AuditEnricher::with_signer(
        Arc::new(SqliteAuditBackend::open(&url).await.unwrap()),
        signer.clone(),
    );
    enricher.seal_record(record("behind")).await.unwrap();
    enricher.seal_record(record("after")).await.unwrap();

    let log = entries(&backend).await;
    let kinds: Vec<&str> = log.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["ahead", "behind", "after"]);
    assert!(log[1].created_at > ahead.created_at);
    verify_chain(&log, &signer).expect("one chain, listed in order");
    assert_eq!(backend.head().await.unwrap(), log[2].entry_hash);
    let _ = std::fs::remove_file(path);
}

/// Entries with the same `created_at` list in insertion order, as the head
/// is read: the listing had no tiebreak, so two entries of one microsecond
/// could list in either order and the chain read as broken.
#[tokio::test]
async fn entries_of_the_same_instant_list_in_chain_order() {
    let path = std::env::temp_dir().join(format!("jj-audit-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let backend = SqliteAuditBackend::open(&url).await.unwrap();
    backend.migrate().await.unwrap();
    let signer = AuditSigner::new(b"k".to_vec());
    let at = chrono::Utc::now();
    let mut prev = None;
    for kind in ["one", "two", "three"] {
        let mut e = jamjet_audit::AuditLogEntry::new(
            uuid::Uuid::new_v4(),
            String::new(),
            0,
            kind.into(),
            "cli".into(),
            ActorType::Human,
            serde_json::json!({ "type": kind }),
        );
        e.created_at = at;
        e.seal(prev.clone(), &signer);
        prev = e.entry_hash.clone();
        backend.append(e).await.unwrap();
    }
    let log = entries(&backend).await;
    let kinds: Vec<&str> = log.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["one", "two", "three"]);
    verify_chain(&log, &signer).expect("listed in chain order");
    assert_eq!(backend.head().await.unwrap(), prev);
    // Filtered by an indexed column the plan sorts in a temp B-tree, where
    // ties come out in whatever order it read them.
    let mut by_actor = backend
        .query(&AuditQuery {
            actor_id: Some("cli".into()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    by_actor.reverse();
    let kinds: Vec<&str> = by_actor.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(kinds, ["one", "two", "three"]);
    let _ = std::fs::remove_file(path);
}
