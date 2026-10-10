//! The runtime publishes its Ed25519 audit key, and a third party holding only
//! that key verifies what `GET /audit` returns, after a round trip through
//! SQLite.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{
    verify_chain, AuditBackend, AuditEnricher, AuditLogEntry, AuditSigner, RequestContext,
    SqliteAuditBackend,
};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend, StateBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

async fn state_with(signer: AuditSigner) -> AppState {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jamjet_audit_ed_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let audit = SqliteAuditBackend::open(&url).await.unwrap();
    audit.migrate().await.unwrap();
    let audit: Arc<dyn AuditBackend> = Arc::new(audit);
    let enricher = Arc::new(AuditEnricher::with_signer(Arc::clone(&audit), signer));
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let b = backend.clone();
    AppState {
        backend,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| b.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
}

async fn get_text(state: &AppState, path: &str) -> String {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{path}: {}", resp.status());
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

async fn seal_some(state: &AppState) {
    let eid = ExecutionId::new();
    for (i, node) in ["a", "b", "c"].iter().enumerate() {
        let event = Event::new(
            eid.clone(),
            i as i64 + 1,
            EventKind::PolicyViolation {
                node_id: node.to_string(),
                rule: "tool 'x' matches blocked pattern 'x'".into(),
                decision: "blocked".into(),
                policy_scope: "workflow".into(),
            },
        );
        state
            .enricher
            .try_enrich_and_append(&event, Some(&RequestContext::system("worker-1")))
            .await
            .unwrap();
    }
}

fn did_public_key(did: &Value) -> Vec<u8> {
    use base64::Engine as _;
    let vm = &did["verificationMethod"][0];
    assert!(vm["id"].as_str().unwrap().ends_with("#audit-key"), "{did}");
    assert_eq!(did["assertionMethod"][0], vm["id"]);
    let jwk = &vm["publicKeyJwk"];
    assert_eq!(jwk["kty"], "OKP");
    assert_eq!(jwk["crv"], "Ed25519");
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(jwk["x"].as_str().unwrap())
        .unwrap()
}

#[tokio::test]
async fn the_published_key_verifies_the_served_log() {
    let state = state_with(AuditSigner::ed25519_from_hex(SEED_HEX).unwrap()).await;
    seal_some(&state).await;

    let did_text = get_text(&state, "/.well-known/did.json").await;
    let audit_text = get_text(&state, "/audit").await;
    for body in [&did_text, &audit_text] {
        assert!(!body.contains(SEED_HEX), "the private key was served");
    }

    let verifier =
        AuditSigner::ed25519_verifier(&did_public_key(&serde_json::from_str(&did_text).unwrap()))
            .unwrap();
    let mut entries: Vec<AuditLogEntry> = serde_json::from_value(
        serde_json::from_str::<Value>(&audit_text).unwrap()["items"].clone(),
    )
    .unwrap();
    assert_eq!(entries.len(), 3);
    // Served newest first; the chain runs oldest first.
    entries.reverse();
    assert_eq!(verify_chain(&entries, &verifier), Ok(()));

    // And it is a real check: an edited entry no longer verifies.
    entries[1].raw_event["node_id"] = json!("z");
    assert!(verify_chain(&entries, &verifier).is_err());
}

#[tokio::test]
async fn hmac_mode_publishes_no_audit_key() {
    let state = state_with(AuditSigner::new(b"shared".to_vec())).await;
    let did: Value =
        serde_json::from_str(&get_text(&state, "/.well-known/did.json").await).unwrap();
    assert!(did.get("verificationMethod").is_none(), "{did}");
    assert!(did.get("assertionMethod").is_none(), "{did}");
    assert!(!did.to_string().contains("shared"));
}
