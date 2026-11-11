//! `AuditQuery` filter values are data, never SQL.
//!
//! `GET /audit` copies its `actor_id`, `event_type` and `execution_id` query
//! parameters into an `AuditQuery`, and the token's tenant into `tenant_id`.
//! The SQLite backend used to splice them into the WHERE clause inside quotes,
//! so a value carrying a quote rewrote the query: it could drop the tenant
//! filter, and a value naming a real actor with a quote in it was a syntax
//! error. Each test here failed against that code.

use chrono::{Duration, Utc};
use jamjet_audit::{ActorType, AuditBackend, AuditLogEntry, AuditQuery, SqliteAuditBackend};
use uuid::Uuid;

/// Turns `x = 'value'` into `x = 'x' OR '1'='1'`.
const OR_TRUE: &str = "x' OR '1'='1";
/// Turns `x = 'value' AND tenant_id = 't' ...` into `x = 'x' OR 1=1`, the
/// rest of the statement (the tenant filter included) commented out.
const OR_TRUE_DROP_REST: &str = "x' OR 1=1 --";

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
    // Another tenant's row: nothing scoped to `default` may return it.
    Row {
        tenant: "acme",
        actor: "carol",
        event_type: "node_started",
        execution_id: "exec-c",
    },
];

async fn seeded() -> SqliteAuditBackend {
    let backend = SqliteAuditBackend::open("sqlite::memory:").await.unwrap();
    backend.migrate().await.unwrap();
    let t0 = Utc::now();
    for (i, r) in ROWS.iter().enumerate() {
        let mut e = AuditLogEntry::new(
            Uuid::new_v4(),
            r.execution_id.to_string(),
            1,
            r.event_type.to_string(),
            r.actor.to_string(),
            ActorType::Agent,
            serde_json::json!({ "type": r.event_type }),
        );
        e.tenant_id = r.tenant.to_string();
        e.created_at = t0 + Duration::seconds(i as i64);
        backend.append(e).await.unwrap();
    }
    backend
}

fn total() -> AuditQuery {
    AuditQuery::new()
}

/// Every row in the store, to check a query left them all in place.
async fn all_rows(b: &SqliteAuditBackend) -> u64 {
    b.count(&total()).await.unwrap()
}

#[derive(Clone, Copy)]
enum Field {
    Actor,
    EventType,
    Execution,
    Tenant,
}

fn query_with(field: Field, value: &str, tenant: Option<&str>) -> AuditQuery {
    let mut q = AuditQuery::new();
    q.tenant_id = tenant.map(str::to_string);
    let value = Some(value.to_string());
    match field {
        Field::Actor => q.actor_id = value,
        Field::EventType => q.event_type = value,
        Field::Execution => q.execution_id = value,
        Field::Tenant => q.tenant_id = value,
    }
    q
}

async fn query_matches_nothing(field: Field, value: &str, tenant: Option<&str>) {
    let b = seeded().await;
    let rows = b
        .query(&query_with(field, value, tenant))
        .await
        .expect("a filter value is data, not SQL");
    let leaked: Vec<_> = rows
        .iter()
        .map(|e| (e.tenant_id.as_str(), e.actor_id.as_str()))
        .collect();
    assert!(leaked.is_empty(), "{value:?} matched rows {leaked:?}");
}

async fn count_matches_nothing(field: Field, value: &str, tenant: Option<&str>) {
    let b = seeded().await;
    let n = b
        .count(&query_with(field, value, tenant))
        .await
        .expect("a filter value is data, not SQL");
    assert_eq!(n, 0, "{value:?} counted {n} rows");
}

// ── query ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn query_actor_id_injection_matches_nothing() {
    query_matches_nothing(Field::Actor, OR_TRUE, None).await;
}

#[tokio::test]
async fn query_event_type_injection_matches_nothing() {
    query_matches_nothing(Field::EventType, OR_TRUE, None).await;
}

#[tokio::test]
async fn query_execution_id_injection_matches_nothing() {
    query_matches_nothing(Field::Execution, OR_TRUE, None).await;
}

#[tokio::test]
async fn query_tenant_id_injection_returns_no_other_tenant() {
    query_matches_nothing(Field::Tenant, "default' OR '1'='1", None).await;
}

#[tokio::test]
async fn query_filter_injection_keeps_the_tenant_filter() {
    for field in [Field::Actor, Field::EventType, Field::Execution] {
        query_matches_nothing(field, OR_TRUE_DROP_REST, Some("default")).await;
    }
}

// ── count ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn count_actor_id_injection_counts_nothing() {
    count_matches_nothing(Field::Actor, OR_TRUE, None).await;
}

#[tokio::test]
async fn count_event_type_injection_counts_nothing() {
    count_matches_nothing(Field::EventType, OR_TRUE, None).await;
}

#[tokio::test]
async fn count_execution_id_injection_counts_nothing() {
    count_matches_nothing(Field::Execution, OR_TRUE, None).await;
}

#[tokio::test]
async fn count_tenant_id_injection_counts_no_other_tenant() {
    count_matches_nothing(Field::Tenant, "default' OR '1'='1", None).await;
}

#[tokio::test]
async fn count_filter_injection_keeps_the_tenant_filter() {
    for field in [Field::Actor, Field::EventType, Field::Execution] {
        count_matches_nothing(field, OR_TRUE_DROP_REST, Some("default")).await;
    }
}

// ── stacked statements ─────────────────────────────────────────────────────

/// SQLite runs every statement in the text it is given, so a closed quote
/// followed by `;` ran a second statement on a read-only query.
#[tokio::test]
async fn a_stacked_statement_in_a_filter_does_not_run() {
    let b = seeded().await;
    let before = all_rows(&b).await;
    let value = "x'; DELETE FROM audit_log; --";
    let _ = b
        .query(&query_with(Field::Actor, value, Some("default")))
        .await;
    let _ = b
        .count(&query_with(Field::Actor, value, Some("default")))
        .await;
    assert_eq!(all_rows(&b).await, before, "the audit log lost rows");
}

// ── a real value with a quote in it ────────────────────────────────────────

#[tokio::test]
async fn a_quoted_actor_id_matches_exactly() {
    let b = seeded().await;
    let q = query_with(Field::Actor, "o'brien", Some("default"));
    let rows = b.query(&q).await.expect("o'brien is an actor id");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor_id, "o'brien");
    assert_eq!(b.count(&q).await.unwrap(), 1);
}

#[tokio::test]
async fn a_quoted_execution_id_matches_exactly() {
    let b = seeded().await;
    let q = query_with(Field::Execution, "exec-'d", Some("default"));
    let rows = b.query(&q).await.expect("exec-'d is an execution id");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].execution_id, "exec-'d");
    assert_eq!(b.count(&q).await.unwrap(), 1);
}

// ── the filters still filter, together and in order ────────────────────────

#[tokio::test]
async fn combined_filters_bind_to_their_own_columns() {
    let b = seeded().await;
    let rows = b.query(&total()).await.unwrap();
    // Newest first: rows were written one second apart.
    let ts: Vec<_> = rows.iter().rev().map(|e| e.created_at).collect();

    let mut q = AuditQuery::new();
    q.from = Some(ts[1]);
    q.to = Some(ts[2]);
    q.tenant_id = Some("default".into());
    let got: Vec<_> = b
        .query(&q)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.actor_id)
        .collect();
    assert_eq!(got, ["o'brien", "bob"]);
    assert_eq!(b.count(&q).await.unwrap(), 2);

    q.actor_id = Some("bob".into());
    q.event_type = Some("policy_violation".into());
    q.execution_id = Some("exec-b".into());
    let got = b.query(&q).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].actor_id, "bob");
    assert_eq!(b.count(&q).await.unwrap(), 1);

    // The same filters scoped to another tenant match nothing.
    q.tenant_id = Some("acme".into());
    assert!(b.query(&q).await.unwrap().is_empty());
    assert_eq!(b.count(&q).await.unwrap(), 0);
}
