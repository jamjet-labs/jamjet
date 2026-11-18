//! `GET /audit` query parameters reach the audit store as bound values.
//!
//! Any token may read `/audit`, a viewer's included. The SQLite audit backend
//! used to splice `actor_id`, `event_type` and `execution_id` into its SQL, so
//! a viewer could rewrite the query: drop the tenant filter and read another
//! tenant's rows, or close the statement and run a second one. The audit log
//! shares the runtime's database (see `main.rs`), and so do these tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{
    ActorType, AuditBackend, AuditEnricher, AuditLogEntry, NoopAuditBackend, SqliteAuditBackend,
};
use jamjet_state::{SqliteBackend, StateBackend};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

struct Env {
    app: axum::Router,
    state: Arc<dyn StateBackend>,
    viewer: String,
}

/// One SQLite file holding the runtime state and an audit log with three of
/// `default`'s rows and one of `acme`'s, behind the authenticated router, and
/// a `viewer` token in `default`.
async fn env() -> Env {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_audit_sqli_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let backend = SqliteBackend::open(&url).await.unwrap();
    let audit = SqliteAuditBackend::open(&url).await.unwrap();
    audit.migrate().await.unwrap();

    let t0 = Utc::now();
    let rows = [
        ("default", "alice", "exec-a"),
        ("default", "bob", "exec-b"),
        ("default", "o'brien", "exec-d"),
        ("acme", "carol", "exec-c"),
    ];
    for (i, (tenant, actor, exec)) in rows.into_iter().enumerate() {
        let mut e = AuditLogEntry::new(
            uuid::Uuid::new_v4(),
            exec.to_string(),
            1,
            "node_started".to_string(),
            actor.to_string(),
            ActorType::Agent,
            serde_json::json!({ "type": "node_started" }),
        );
        e.tenant_id = tenant.to_string();
        e.created_at = t0 + Duration::seconds(i as i64);
        audit.append(e).await.unwrap();
    }

    let (viewer, _) = backend.create_token("auditor", "viewer").await.unwrap();
    let backend: Arc<dyn StateBackend> = Arc::new(backend);
    let b = backend.clone();
    let audit: Arc<dyn AuditBackend> = Arc::new(audit);
    // The enricher is not under test; sealing nothing keeps the seeded rows
    // the only rows in the store.
    let noop: Arc<dyn AuditBackend> = Arc::new(NoopAuditBackend);
    let state = AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| b.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher: Arc::new(AuditEnricher::new(noop)),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    Env {
        app: build_router_with_opts(state, false),
        state: backend,
        viewer,
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn get_audit(env: &Env, param: &str, value: &str) -> (StatusCode, Value) {
    let uri = format!("/audit?{param}={}", encode(value));
    let req = Request::get(&uri)
        .header("authorization", format!("Bearer {}", env.viewer))
        .body(Body::empty())
        .unwrap();
    let resp = env.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn actors(body: &Value) -> Vec<(String, String)> {
    body["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|e| {
                    (
                        e["tenant_id"].as_str().unwrap_or("").to_string(),
                        e["actor_id"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_viewer_cannot_rewrite_the_audit_query() {
    let env = env().await;
    for param in ["actor_id", "event_type", "execution_id"] {
        for value in ["x' OR 1=1 --", "x' OR '1'='1"] {
            let (status, body) = get_audit(&env, param, value).await;
            assert_eq!(status, StatusCode::OK, "{param}={value:?}: {body}");
            assert_eq!(
                actors(&body),
                Vec::<(String, String)>::new(),
                "{param}={value:?} matched rows"
            );
            assert_eq!(body["total"], 0, "{param}={value:?}: {body}");
        }
    }
}

#[tokio::test]
async fn a_quoted_actor_id_matches_exactly_over_http() {
    let env = env().await;
    let (status, body) = get_audit(&env, "actor_id", "o'brien").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        actors(&body),
        vec![("default".to_string(), "o'brien".to_string())]
    );
    assert_eq!(body["total"], 1);
}

/// SQLite runs every statement in the text it is given, so a value that
/// closed its quote and added `;` ran a second statement on the runtime's
/// database.
#[tokio::test]
async fn a_viewer_cannot_change_a_token_role() {
    let env = env().await;
    let value = "x'; UPDATE api_tokens SET role = 'operator'; --";
    let (status, body) = get_audit(&env, "actor_id", value).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0, "{body}");

    let token = env
        .state
        .validate_token(&env.viewer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(token.role, "viewer", "a viewer query changed its own role");
}

#[tokio::test]
async fn a_viewer_cannot_delete_audit_rows() {
    let env = env().await;
    let value = "x'; DELETE FROM audit_log; --";
    let (status, body) = get_audit(&env, "actor_id", value).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 0, "{body}");

    // `default`'s three rows; `acme`'s row is out of this token's scope.
    let (_, body) = get_audit(&env, "limit", "50").await;
    assert_eq!(
        body["total"], 3,
        "a viewer query deleted audit rows: {body}"
    );
}
