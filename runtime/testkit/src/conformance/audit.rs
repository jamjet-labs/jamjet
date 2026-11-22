//! One behaviour suite for every audit backend that keeps a chain: SQLite
//! and Postgres.
//!
//! [`audit_conformance_suite!`](crate::audit_conformance_suite) turns every
//! case in [`cases`] into a test on one [`AuditFactory`], so a backend's
//! suite is one line in `testkit/tests/`:
//!
//! ```text
//! jamjet_testkit::audit_conformance_suite!(jamjet_testkit::conformance::audit::SqliteAuditFactory);
//! ```

use super::factories::{connect_pg, TempDir};
use crate::pg::TestDatabase;
use async_trait::async_trait;
use jamjet_audit::{AuditBackend, PgAuditBackend, SqliteAuditBackend};
use std::sync::Arc;

/// A fresh audit log for one case. Fields drop in order, so `keep` goes
/// last, after every pool on it.
pub struct AuditFixture {
    /// The log the case writes.
    pub backend: Arc<dyn AuditBackend>,
    /// The same log through a pool of its own, as a second process sees it.
    pub second: Arc<dyn AuditBackend>,
    /// What keeps the log alive: a temp directory or a test database.
    pub keep: Box<dyn std::any::Any + Send + Sync>,
}

/// Makes a fresh [`AuditFixture`] per case.
#[async_trait]
pub trait AuditFactory: Send + Sync {
    /// `None` means the store is not configured here and the case skips.
    async fn fresh(&self) -> Option<AuditFixture>;
}

/// A fresh SQLite file in a temp directory with the audit table, opened
/// twice.
pub struct SqliteAuditFactory;

#[async_trait]
impl AuditFactory for SqliteAuditFactory {
    async fn fresh(&self) -> Option<AuditFixture> {
        let dir = TempDir::new();
        let url = format!("sqlite://{}?mode=rwc", dir.0.join("audit.db").display());
        let open = || async {
            SqliteAuditBackend::open(&url)
                .await
                .unwrap_or_else(|e| panic!("open the SQLite file at {url}: {e}"))
        };
        let backend = open().await;
        backend
            .migrate()
            .await
            .unwrap_or_else(|e| panic!("create the audit table: {e}"));
        let second = open().await;
        Some(AuditFixture {
            backend: Arc::new(backend),
            second: Arc::new(second),
            keep: Box::new(dir),
        })
    }
}

/// A fresh Postgres database of its own, connected twice as the engine
/// connects. `None` when no Postgres server is configured.
pub struct PostgresAuditFactory;

#[async_trait]
impl AuditFactory for PostgresAuditFactory {
    async fn fresh(&self) -> Option<AuditFixture> {
        let db = TestDatabase::fresh().await?;
        let backend = PgAuditBackend::new(connect_pg(db.url()).await.pool());
        let second = PgAuditBackend::new(connect_pg(db.url()).await.pool());
        Some(AuditFixture {
            backend: Arc::new(backend),
            second: Arc::new(second),
            keep: Box::new(db),
        })
    }
}

/// Expands every case in [`cases`] into a test on `$factory`, in a module
/// named `audit`.
#[macro_export]
macro_rules! audit_conformance_suite {
    ($factory:expr) => {
        $crate::__audit_conformance_module!(
            $factory,
            [
                a_linked_append_onto_a_stale_head_is_refused,
                a_linked_append_dated_at_or_before_the_head_is_refused,
                head_at_and_query_agree,
                an_entry_reads_back_as_it_was_sealed,
                an_entry_appended_twice_is_kept_once,
                filter_values_are_bound_not_spliced,
            ]
        );
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __audit_conformance_module {
    ($factory:expr, [$($case:ident),* $(,)?]) => {
        mod audit {
            #[allow(unused_imports)]
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let Some(fx) =
                        $crate::conformance::audit::AuditFactory::fresh(&$factory).await
                    else {
                        return $crate::skip(stringify!($case));
                    };
                    $crate::conformance::audit::cases::$case(&fx).await;
                }
            )*
        }
    };
}

/// The cases. Times are whole microseconds, which both stores keep.
pub mod cases {
    use super::AuditFixture;
    use chrono::{DateTime, Duration, SubsecRound, Utc};
    use jamjet_audit::{
        verify_chain, verify_entry, ActorType, AuditBackend, AuditLogEntry, AuditQuery, AuditSigner,
    };
    use uuid::Uuid;

    fn signer() -> AuditSigner {
        AuditSigner::new(b"conformance".to_vec())
    }

    /// Now, to the microsecond.
    fn now() -> DateTime<Utc> {
        Utc::now().trunc_subsecs(6)
    }

    fn entry(kind: &str, at: DateTime<Utc>) -> AuditLogEntry {
        let mut e = AuditLogEntry::new(
            Uuid::new_v4(),
            format!("exec-{kind}"),
            1,
            kind.into(),
            "cli".into(),
            ActorType::Human,
            serde_json::json!({ "type": kind }),
        );
        e.created_at = at;
        e
    }

    fn sealed(kind: &str, prev: Option<String>, at: DateTime<Utc>) -> AuditLogEntry {
        let mut e = entry(kind, at);
        e.seal(prev, &signer());
        e
    }

    /// Every entry, oldest first.
    async fn chain(b: &dyn AuditBackend) -> Vec<AuditLogEntry> {
        let mut all = b
            .query(&AuditQuery {
                limit: 1_000,
                ..Default::default()
            })
            .await
            .unwrap();
        all.reverse();
        all
    }

    fn kinds(log: &[AuditLogEntry]) -> Vec<&str> {
        log.iter().map(|e| e.event_type.as_str()).collect()
    }

    /// An entry sealed onto a head another writer has moved on from is
    /// refused, from either handle, and nothing is written.
    pub async fn a_linked_append_onto_a_stale_head_is_refused(fx: &AuditFixture) {
        let t0 = now();
        assert_eq!(fx.backend.head_at().await.unwrap(), None);
        let one = sealed("one", None, t0);
        assert!(fx.backend.append_linked(one.clone()).await.unwrap());

        let onto_nothing = sealed("two", None, t0 + Duration::milliseconds(1));
        assert!(
            !fx.second.append_linked(onto_nothing).await.unwrap(),
            "sealed onto the empty log the second handle last saw"
        );
        let two = sealed(
            "two",
            one.entry_hash.clone(),
            t0 + Duration::milliseconds(1),
        );
        assert!(fx.second.append_linked(two).await.unwrap());

        let onto_one = sealed(
            "three",
            one.entry_hash.clone(),
            t0 + Duration::milliseconds(2),
        );
        assert!(
            !fx.backend.append_linked(onto_one).await.unwrap(),
            "sealed onto a head the other handle moved on from"
        );
        let log = chain(&*fx.backend).await;
        assert_eq!(kinds(&log), ["one", "two"]);
        verify_chain(&log, &signer()).expect("one chain");
    }

    /// An entry linked onto the head but dated before it, or at the same
    /// instant, is refused; one microsecond after is taken.
    pub async fn a_linked_append_dated_at_or_before_the_head_is_refused(fx: &AuditFixture) {
        let t0 = now();
        let one = sealed("one", None, t0);
        assert!(fx.backend.append_linked(one.clone()).await.unwrap());
        for (at, when) in [(t0 - Duration::seconds(1), "before"), (t0, "at")] {
            let late = sealed("late", one.entry_hash.clone(), at);
            assert!(
                !fx.second.append_linked(late).await.unwrap(),
                "linked onto the head, dated {when} it"
            );
        }
        let next = sealed(
            "next",
            one.entry_hash.clone(),
            t0 + Duration::microseconds(1),
        );
        assert!(fx.second.append_linked(next).await.unwrap());
        assert_eq!(fx.backend.count(&AuditQuery::new()).await.unwrap(), 2);
    }

    /// `head_at` and `head` name the entry `query` lists first, on both
    /// handles, from the empty log on; `query` lists newest first and pages
    /// with `offset`.
    pub async fn head_at_and_query_agree(fx: &AuditFixture) {
        for b in [&fx.backend, &fx.second] {
            assert_eq!(b.head_at().await.unwrap(), None);
            assert_eq!(b.head().await.unwrap(), None);
            assert!(b.query(&AuditQuery::new()).await.unwrap().is_empty());
            assert_eq!(b.count(&AuditQuery::new()).await.unwrap(), 0);
        }
        let t0 = now();
        let mut prev = None;
        for (n, kind) in ["one", "two", "three"].into_iter().enumerate() {
            let writer = if n % 2 == 0 { &fx.backend } else { &fx.second };
            let e = sealed(kind, prev.clone(), t0 + Duration::milliseconds(n as i64));
            assert!(writer.append_linked(e.clone()).await.unwrap());
            prev = e.entry_hash;
        }
        for b in [&fx.backend, &fx.second] {
            let newest = b
                .query(&AuditQuery {
                    limit: 1,
                    ..Default::default()
                })
                .await
                .unwrap()
                .remove(0);
            assert_eq!(newest.event_type, "three");
            assert_eq!(
                b.head_at().await.unwrap(),
                Some((newest.entry_hash.clone(), newest.created_at))
            );
            assert_eq!(b.head().await.unwrap(), prev);
            assert_eq!(b.count(&AuditQuery::new()).await.unwrap(), 3);
            let middle = b
                .query(&AuditQuery {
                    limit: 1,
                    offset: 1,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(kinds(&middle), ["two"]);
        }
        let log = chain(&*fx.backend).await;
        assert_eq!(kinds(&log), ["one", "two", "three"]);
        verify_chain(&log, &signer()).expect("listed in chain order");
    }

    /// Every field reads back as it was sealed, so the entry verifies.
    pub async fn an_entry_reads_back_as_it_was_sealed(fx: &AuditFixture) {
        let mut e = entry("approval_received", now());
        e.tool_call_hash = Some("ab12".into());
        e.policy_decision = Some("require_approval".into());
        e.http_request_id = Some("req-1".into());
        e.http_method = Some("POST".into());
        e.http_path = Some("/executions/x/approve".into());
        e.ip_address = Some("10.0.0.1".into());
        e.tenant_id = "acme".into();
        e.expires_at = Some(e.created_at + Duration::days(30));
        e.redacted = true;
        e.raw_event =
            serde_json::json!({ "type": "approval_received", "note": "a \u{0} b", "n": 1.5 });
        e.seal(None, &signer());
        assert!(fx.backend.append_linked(e.clone()).await.unwrap());

        let back = chain(&*fx.second).await.remove(0);
        verify_entry(&back, &signer()).expect("reads back as sealed");
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&e).unwrap()
        );
    }

    /// Appending one entry again (a replay after a crash) is no error and
    /// keeps one copy, linked or not.
    pub async fn an_entry_appended_twice_is_kept_once(fx: &AuditFixture) {
        let t0 = now();
        let one = sealed("one", None, t0);
        fx.backend.append(one.clone()).await.unwrap();
        fx.second
            .append(one.clone())
            .await
            .expect("a second append of one entry is no error");
        let two = sealed(
            "two",
            one.entry_hash.clone(),
            t0 + Duration::milliseconds(1),
        );
        assert!(fx.backend.append_linked(two.clone()).await.unwrap());
        fx.second
            .append(two)
            .await
            .expect("a second append of the head is no error");
        let log = chain(&*fx.backend).await;
        assert_eq!(kinds(&log), ["one", "two"]);
        verify_chain(&log, &signer()).expect("one chain");
    }

    struct Row {
        tenant: &'static str,
        actor: &'static str,
        event_type: &'static str,
        execution_id: &'static str,
    }

    const ROWS: &[Row] = &[
        Row {
            tenant: "default",
            actor: "alice",
            event_type: "node_started",
            execution_id: "exec-a",
        },
        Row {
            tenant: "default",
            actor: "bob",
            event_type: "policy_violation",
            execution_id: "exec-b",
        },
        Row {
            tenant: "default",
            actor: "o'brien",
            event_type: "approval_received",
            execution_id: "exec-'d",
        },
        Row {
            tenant: "acme",
            actor: "carol",
            event_type: "node_started",
            execution_id: "exec-c",
        },
    ];

    /// A filter value is data, never SQL: a value with a quote matches
    /// exactly, values written to rewrite the query match and count
    /// nothing and keep the tenant filter, a stacked statement does not
    /// run, and the filters combine on their own columns.
    pub async fn filter_values_are_bound_not_spliced(fx: &AuditFixture) {
        let b = &fx.backend;
        let t0 = now();
        let mut prev = None;
        for (i, r) in ROWS.iter().enumerate() {
            let mut e = AuditLogEntry::new(
                Uuid::new_v4(),
                r.execution_id.into(),
                1,
                r.event_type.into(),
                r.actor.into(),
                ActorType::Agent,
                serde_json::json!({ "type": r.event_type }),
            );
            e.tenant_id = r.tenant.into();
            e.created_at = t0 + Duration::seconds(i as i64);
            e.seal(prev.clone(), &signer());
            prev = e.entry_hash.clone();
            assert!(b.append_linked(e).await.unwrap());
        }
        let total = ROWS.len() as u64;

        let mut quoted = AuditQuery::new();
        quoted.actor_id = Some("o'brien".into());
        quoted.tenant_id = Some("default".into());
        let rows = b.query(&quoted).await.expect("o'brien is an actor id");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor_id, "o'brien");
        assert_eq!(b.count(&quoted).await.unwrap(), 1);

        let hostile = [
            ("x' OR '1'='1", None),
            ("x' OR 1=1 --", Some("default")),
            ("x'; DELETE FROM audit_log; --", Some("default")),
        ];
        for (value, tenant) in hostile {
            for field in 0..3 {
                let mut q = AuditQuery::new();
                q.tenant_id = tenant.map(str::to_string);
                let v = Some(value.to_string());
                match field {
                    0 => q.actor_id = v,
                    1 => q.event_type = v,
                    _ => q.execution_id = v,
                }
                let rows = b.query(&q).await.expect("a filter value is data");
                assert!(rows.is_empty(), "{value:?} matched {} rows", rows.len());
                assert_eq!(b.count(&q).await.unwrap(), 0, "{value:?} counted rows");
            }
        }
        let mut tenant = AuditQuery::new();
        tenant.tenant_id = Some("default' OR '1'='1".into());
        assert!(b.query(&tenant).await.unwrap().is_empty());
        assert_eq!(b.count(&tenant).await.unwrap(), 0);
        assert_eq!(b.count(&AuditQuery::new()).await.unwrap(), total);

        let mut q = AuditQuery::new();
        q.from = Some(t0 + Duration::seconds(1));
        q.to = Some(t0 + Duration::seconds(2));
        q.tenant_id = Some("default".into());
        let actors: Vec<String> = b
            .query(&q)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.actor_id)
            .collect();
        assert_eq!(actors, ["o'brien", "bob"]);
        assert_eq!(b.count(&q).await.unwrap(), 2);
        q.actor_id = Some("bob".into());
        q.event_type = Some("policy_violation".into());
        q.execution_id = Some("exec-b".into());
        assert_eq!(b.query(&q).await.unwrap().len(), 1);
        assert_eq!(b.count(&q).await.unwrap(), 1);
        q.tenant_id = Some("acme".into());
        assert!(b.query(&q).await.unwrap().is_empty());
        assert_eq!(b.count(&q).await.unwrap(), 0);
    }
}
