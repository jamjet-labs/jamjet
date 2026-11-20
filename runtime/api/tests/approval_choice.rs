//! A `human_approval` node can offer options, and approving it names one.
//!
//! The request shows the node's options. An approval of a node with options
//! needs a `choice` among them, compared byte for byte; a choice anywhere
//! else (a node without options, a rejection) is refused. Every decision can
//! carry a free `note`, with `comment` as its old name: at most 4096 bytes,
//! no control character but newline, carriage return and tab, and no
//! invisible format character. A blank note is no note.
//!
//! A request whose options are not the ones the run's workflow declares for
//! the node is refused whatever the choice. Only a doctored log or a bug
//! writes one, so these tests append it through the backend.
//!
//! The deadline and the digest are checked before the choice, so a late
//! decision with a bad choice is still expired.
//!
//! The approval node writes the choice and the note into the run's state
//! (`state.__approvals.<node>.choice`, `.note`), read from the decision on
//! record, so a condition can route on the choice and no worker can change
//! it. The decision is sealed into the audit log whole, choice included.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_audit::{
    verify_chain, AuditBackend, AuditEnricher, AuditLogEntry, AuditQuery, AuditSigner, ChainError,
    SqliteAuditBackend,
};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_scheduler::{Projector, Scheduler};
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use jamjet_worker::{ExecutionResult, ExecutorError, NodeExecutor, Worker};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

const WORKFLOW: &str = "wf-topics";

/// What a request on `pick_topic` with `topics()` refuses an approval with
/// when its choice is missing.
const CHOICE_REQUIRED: &str =
    "approving this request needs a choice, one of: topic_1, topic_2, revise";
/// ... and when its choice is not one of them.
const CHOICE_NOT_OFFERED: &str =
    "the choice is not one of this request's options: topic_1, topic_2, revise";
const NO_OPTIONS: &str = "this request offers no options, so a decision on it takes no choice";
const REJECTION_CHOICE: &str = "a rejection takes no choice";
const BAD_CHARACTER: &str = "the note may not hold a control character other than newline, \
     carriage return and tab, or an invisible format character";
const NOTE_AND_COMMENT: &str =
    "note and comment differ; comment is the old name of note, send one of them";
const OPTIONS_CHANGED: &str = "the options in this request are not the ones the run's \
     workflow declares for the node; nothing was recorded";

/// The three options the tests offer, in order.
fn topics() -> Value {
    json!([
        { "id": "topic_1", "label": "Error handling in Rust" },
        { "id": "topic_2", "label": "Durable timers" },
        { "id": "revise", "label": "None of these, revise the list" }
    ])
}

/// `pick_topic` (human_approval, with `options` when given) -> `publish`.
/// A node without options is its own workflow, so one test can register both.
fn ir(options: Option<Value>, timeout_secs: Option<u64>) -> Value {
    let workflow_id = if options.is_some() {
        WORKFLOW
    } else {
        "wf-plain"
    };
    let mut kind = json!({
        "type": "human_approval",
        "description": "Which topic goes out this week?",
        "timeout_secs": timeout_secs,
        "fallback_node": null
    });
    if let Some(options) = options {
        kind["options"] = options;
    }
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pick_topic",
        "nodes": {
            "pick_topic": {
                "id": "pick_topic", "kind": kind,
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            },
            "publish": {
                "id": "publish",
                "kind": { "type": "python_fn", "module": "myapp.posts", "function": "publish", "output_schema": "" },
                "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
            }
        },
        "edges": [
            { "from": "pick_topic", "to": "publish", "condition": null },
            { "from": "publish", "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

struct Env {
    name: &'static str,
    backend: Arc<dyn StateBackend>,
    app: axum::Router,
    /// Starts the runs.
    alice: String,
    /// Decides them.
    bob: String,
    engine: Worker,
    scheduler: Scheduler,
}

/// One environment per backend (in-memory and SQLite): the production router
/// with auth on, and the server's own in-process executors.
async fn envs() -> Vec<Env> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        let state = support::make_state(tb.dyn_backend());
        out.push(env_on(tb.name(), state, false).await);
    }
    out
}

/// An environment serving `state`. With `seal_requests` the engine seals the
/// requests it raises into the state's audit log, as the server does.
async fn env_on(name: &'static str, state: AppState, seal_requests: bool) -> Env {
    let backend = state.backend.clone();
    let mut engine = jamjet_worker::server_pool(
        backend.clone(),
        Arc::new(jamjet_models::ModelRegistry::new()),
    )
    .build_worker("engine".into(), vec!["general".into()]);
    if seal_requests {
        engine = engine.with_sealer(state.enricher.clone());
    }
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    Env {
        name,
        scheduler: Scheduler::new(backend.clone()),
        backend,
        app,
        alice,
        bob,
        engine,
    }
}

async fn call(env: &Env, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Register `ir` (again: the same definition is a 200) and start a run of it
/// as alice, without letting anything run.
async fn start(env: &Env, ir: &Value) -> ExecutionId {
    let (st, body) = call(env, "/workflows", &env.alice, json!({ "ir": ir })).await;
    assert!(st.is_success(), "{}: register: {st} {body}", env.name);
    let (st, body) = call(
        env,
        "/executions",
        &env.alice,
        json!({ "workflow_id": ir["workflow_id"], "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

async fn settle(env: &Env) {
    for _ in 0..6 {
        let _ = env.engine.poll_and_execute().await;
        env.scheduler.tick().await.unwrap();
    }
}

async fn events(env: &Env, eid: &ExecutionId) -> Vec<Event> {
    env.backend.get_events(eid).await.unwrap()
}

/// The latest request on `pick_topic`: its context and sequence.
fn latest_request(events: &[Event]) -> (Value, i64) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired {
                node_id, context, ..
            } if node_id == "pick_topic" => Some((context.clone(), e.sequence)),
            _ => None,
        })
        .expect("pick_topic raised an approval request")
}

/// A run parked on its approval request.
struct Parked {
    eid: ExecutionId,
    context: Value,
    digest: String,
}

/// Start a run of `ir` and let it reach the request.
async fn park(env: &Env, ir: &Value) -> Parked {
    let eid = start(env, ir).await;
    settle(env).await;
    let (context, _) = latest_request(&events(env, &eid).await);
    let digest = context["action_digest"]
        .as_str()
        .unwrap_or_else(|| panic!("{}: bound to the action: {context}", env.name))
        .to_string();
    Parked {
        eid,
        context,
        digest,
    }
}

fn approve_path(eid: &ExecutionId) -> String {
    format!("/executions/exec_{}/approve", eid.0.simple())
}

/// An approval of `pick_topic` echoing `digest`, with `extra` merged in.
fn approval(digest: &str, extra: Value) -> Value {
    let mut body =
        json!({ "decision": "approved", "node_id": "pick_topic", "action_digest": digest });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

/// A rejection of `pick_topic`, with `extra` merged in.
fn rejection(extra: Value) -> Value {
    let mut body = json!({ "decision": "rejected", "node_id": "pick_topic" });
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

async fn decide(env: &Env, parked: &Parked, body: Value) -> (StatusCode, Value) {
    call(env, &approve_path(&parked.eid), &env.bob, body).await
}

/// Every `ApprovalReceived` of the run, as it is stored.
async fn decisions(env: &Env, eid: &ExecutionId) -> Vec<Value> {
    events(env, eid)
        .await
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .map(|e| serde_json::to_value(&e.kind).unwrap())
        .collect()
}

/// The run's one decision.
async fn the_decision(env: &Env, eid: &ExecutionId) -> Value {
    let mut all = decisions(env, eid).await;
    assert_eq!(all.len(), 1, "{}: one decision: {all:?}", env.name);
    all.remove(0)
}

/// `body` is refused with `status` and exactly `error`.
fn assert_refused(env: &Env, (st, body): (StatusCode, Value), status: StatusCode, error: &str) {
    assert_eq!(st, status, "{}: {body}", env.name);
    assert_eq!(body["error"], error, "{}", env.name);
}

/// Append a request for `pick_topic` through the backend, the way a doctored
/// log or a bug would: `context` is the parked request's, changed by `edit`.
/// Returns the sequence the backend gave the new request.
async fn append_request(env: &Env, parked: &Parked, edit: impl FnOnce(&mut Value)) -> i64 {
    let mut context = parked.context.clone();
    edit(&mut context);
    let seq = env.backend.latest_sequence(&parked.eid).await.unwrap() + 1;
    env.backend
        .append_event(Event::new(
            parked.eid.clone(),
            seq,
            EventKind::ToolApprovalRequired {
                node_id: "pick_topic".into(),
                tool_name: "pick_topic".into(),
                approver: "human".into(),
                context,
            },
        ))
        .await
        .unwrap()
}

/// Each decision is refused with 409 `OPTIONS_CHANGED` on the request at
/// `sequence`, nothing is recorded, and then the same request appended
/// without options is approved with no choice.
async fn options_refused_then_plain_decided(env: &Env, parked: &Parked, sequence: i64) {
    for (label, mut body) in [
        (
            "an approval naming an offered id",
            approval(&parked.digest, json!({ "choice": "topic_2" })),
        ),
        ("a rejection", rejection(json!({}))),
    ] {
        body["request_sequence"] = json!(sequence);
        let (st, answer) = decide(env, parked, body).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {label}: {answer}", env.name);
        assert_eq!(answer["error"], OPTIONS_CHANGED, "{}: {label}", env.name);
    }
    assert!(decisions(env, &parked.eid).await.is_empty(), "{}", env.name);

    // Control: the same request without the options is decided.
    let sequence = append_request(env, parked, |c| {
        c.as_object_mut().unwrap().remove("options");
    })
    .await;
    let mut body = approval(&parked.digest, json!({}));
    body["request_sequence"] = json!(sequence);
    let (st, answer) = decide(env, parked, body).await;
    assert_eq!(st, StatusCode::OK, "{}: {answer}", env.name);
    let decision = the_decision(env, &parked.eid).await;
    assert_eq!(decision["request_sequence"], sequence, "{}", env.name);
    assert!(decision.get("choice").is_none(), "{}: {decision}", env.name);
}

#[tokio::test]
async fn the_request_shows_the_options() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        assert_eq!(parked.context["options"], topics(), "{}", env.name);
        assert_eq!(
            parked.context["description"], "Which topic goes out this week?",
            "{}",
            env.name
        );
        let plain = park(&env, &ir(None, None)).await;
        assert!(
            plain.context.get("options").is_none(),
            "{}: a node without options shows none: {}",
            env.name,
            plain.context
        );
    }
}

#[tokio::test]
async fn an_approval_without_a_choice_on_a_node_with_options_is_400() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        for extra in [json!({}), json!({ "choice": null })] {
            let answer = decide(&env, &parked, approval(&parked.digest, extra)).await;
            assert_refused(&env, answer, StatusCode::BAD_REQUEST, CHOICE_REQUIRED);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn a_choice_not_on_the_list_is_400() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        for choice in [
            "topic_9",
            "",
            "Topic_2",
            "TOPIC_2",
            "topic_2 ",
            " topic_2",
            "Durable timers",
        ] {
            let answer = decide(
                &env,
                &parked,
                approval(&parked.digest, json!({ "choice": choice })),
            )
            .await;
            assert_refused(&env, answer, StatusCode::BAD_REQUEST, CHOICE_NOT_OFFERED);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        // The id itself is taken, and recorded.
        let (st, body) = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_2" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["choice"], "topic_2", "{}: {decision}", env.name);
    }
}

#[tokio::test]
async fn a_choice_on_a_node_without_options_is_400() {
    for env in envs().await {
        let parked = park(&env, &ir(None, None)).await;
        for choice in ["topic_2", ""] {
            let answer = decide(
                &env,
                &parked,
                approval(&parked.digest, json!({ "choice": choice })),
            )
            .await;
            assert_refused(&env, answer, StatusCode::BAD_REQUEST, NO_OPTIONS);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        let (st, body) = decide(&env, &parked, approval(&parked.digest, json!({}))).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert!(decision.get("choice").is_none(), "{}: {decision}", env.name);
    }
}

#[tokio::test]
async fn a_rejection_with_a_choice_is_400() {
    for env in envs().await {
        for options in [Some(topics()), None] {
            let parked = park(&env, &ir(options, None)).await;
            for choice in ["topic_1", "revise"] {
                let answer = decide(&env, &parked, rejection(json!({ "choice": choice }))).await;
                assert_refused(&env, answer, StatusCode::BAD_REQUEST, REJECTION_CHOICE);
            }
            assert!(
                decisions(&env, &parked.eid).await.is_empty(),
                "{}",
                env.name
            );

            let (st, body) = decide(&env, &parked, rejection(json!({}))).await;
            assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
            let decision = the_decision(&env, &parked.eid).await;
            assert_eq!(decision["decision"], "rejected", "{}", env.name);
            assert!(decision.get("choice").is_none(), "{}: {decision}", env.name);
        }
    }
}

#[tokio::test]
async fn a_rejection_keeps_its_note() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let note = "Neither topic fits this week.";
        let (st, body) = decide(&env, &parked, rejection(json!({ "note": note }))).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["decision"], "rejected", "{}", env.name);
        assert_eq!(decision["comment"], note, "{}: {decision}", env.name);
        assert!(decision.get("choice").is_none(), "{}: {decision}", env.name);
    }
}

/// Bytes, not characters: both notes are 1,366 or 1,367 characters, far under
/// 4096, and the last character of the long one straddles the limit.
#[tokio::test]
async fn a_note_of_4096_bytes_is_kept_and_4097_is_400() {
    let euros = "\u{20ac}".repeat(1365);
    let kept = format!("a{euros}");
    let long = format!("ab{euros}");
    assert_eq!((kept.len(), long.len()), (4096, 4097));
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        for field in ["note", "comment"] {
            let answer = decide(
                &env,
                &parked,
                approval(&parked.digest, json!({ "choice": "topic_1", field: long })),
            )
            .await;
            assert_refused(
                &env,
                answer,
                StatusCode::BAD_REQUEST,
                "the note is 4097 bytes; a note is at most 4096 bytes",
            );
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        let (st, body) = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_1", "note": kept })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["comment"], kept.as_str(), "{}", env.name);
    }
}

#[tokio::test]
async fn a_note_with_a_nul_or_a_bidi_override_is_400() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        for note in [
            "Fine\u{0}",
            "Fine \u{202e}txt.exe",
            "Fine\u{200b}",
            "Fine\u{7}",
        ] {
            for field in ["note", "comment"] {
                let answer = decide(
                    &env,
                    &parked,
                    approval(&parked.digest, json!({ "choice": "topic_1", field: note })),
                )
                .await;
                assert_refused(&env, answer, StatusCode::BAD_REQUEST, BAD_CHARACTER);
                let answer = decide(&env, &parked, rejection(json!({ field: note }))).await;
                assert_refused(&env, answer, StatusCode::BAD_REQUEST, BAD_CHARACTER);
            }
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn a_note_with_newlines_and_tabs_is_kept() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let note = "Go with the second one.\nKeep it short.\r\n\tCut the intro.";
        let (st, body) = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_2", "note": note })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["comment"], note, "{}", env.name);
    }
}

#[tokio::test]
async fn an_empty_or_blank_note_is_recorded_as_none() {
    for env in envs().await {
        for (field, blank) in [("note", ""), ("note", " \n\t\r "), ("comment", "   ")] {
            let parked = park(&env, &ir(Some(topics()), None)).await;
            let (st, body) = decide(
                &env,
                &parked,
                approval(&parked.digest, json!({ "choice": "topic_1", field: blank })),
            )
            .await;
            assert_eq!(
                st,
                StatusCode::OK,
                "{}: {field} {blank:?}: {body}",
                env.name
            );
            let decision = the_decision(&env, &parked.eid).await;
            assert_eq!(
                decision["comment"],
                Value::Null,
                "{}: {field} {blank:?}: {decision}",
                env.name
            );
        }
    }
}

#[tokio::test]
async fn note_and_comment_that_differ_are_400() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        for (note, comment) in [("Ship it.", "Hold it."), ("Ship it.", ""), ("", " ")] {
            let answer = decide(
                &env,
                &parked,
                approval(
                    &parked.digest,
                    json!({ "choice": "topic_1", "note": note, "comment": comment }),
                ),
            )
            .await;
            assert_refused(&env, answer, StatusCode::BAD_REQUEST, NOTE_AND_COMMENT);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        // The same text under both names is one note.
        let (st, body) = decide(
            &env,
            &parked,
            approval(
                &parked.digest,
                json!({ "choice": "topic_1", "note": "Ship it.", "comment": "Ship it." }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert_eq!(
            the_decision(&env, &parked.eid).await["comment"],
            "Ship it.",
            "{}",
            env.name
        );

        // `comment` alone is the note, from a client that predates `note`.
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let (st, body) = decide(
            &env,
            &parked,
            approval(
                &parked.digest,
                json!({ "choice": "topic_1", "comment": "Old client." }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        assert_eq!(
            the_decision(&env, &parked.eid).await["comment"],
            "Old client.",
            "{}",
            env.name
        );
    }
}

/// A request appended through the backend whose options differ from the
/// node's in the run's workflow: another option, a changed label, another
/// order, or none at all. Each is refused, even with a choice the request
/// itself offers.
#[tokio::test]
async fn options_changed_between_request_and_decision_is_409() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let mut added = topics();
        added
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "topic_9", "label": "Something else" }));
        let mut relabelled = topics();
        relabelled[1]["label"] = json!("Durable timers, part two");
        let mut reordered = topics();
        reordered.as_array_mut().unwrap().swap(0, 1);
        let mut annotated = topics();
        annotated[1]["hint"] = json!("Recommended");
        let cases: Vec<(Option<Value>, Value)> = vec![
            (Some(added), json!({ "choice": "topic_9" })),
            (Some(relabelled), json!({ "choice": "topic_2" })),
            (Some(reordered), json!({ "choice": "topic_2" })),
            (Some(annotated), json!({ "choice": "topic_2" })),
            (Some(json!("topic_2")), json!({ "choice": "topic_2" })),
            (None, json!({})),
        ];
        for (options, extra) in cases {
            let label = format!("{options:?}");
            let sequence = append_request(&env, &parked, |c| match options {
                Some(o) => c["options"] = o,
                None => {
                    c.as_object_mut().unwrap().remove("options");
                }
            })
            .await;
            let mut body = approval(&parked.digest, extra);
            body["request_sequence"] = json!(sequence);
            let answer = decide(&env, &parked, body).await;
            assert_eq!(
                answer.0,
                StatusCode::CONFLICT,
                "{}: {label}: {}",
                env.name,
                answer.1
            );
            assert_eq!(answer.1["error"], OPTIONS_CHANGED, "{}: {label}", env.name);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        // Control: a request appended the same way with the node's own
        // options is decided.
        let sequence = append_request(&env, &parked, |_| {}).await;
        let mut body = approval(&parked.digest, json!({ "choice": "topic_2" }));
        body["request_sequence"] = json!(sequence);
        let (st, answer) = decide(&env, &parked, body).await;
        assert_eq!(st, StatusCode::OK, "{}: {answer}", env.name);
        assert_eq!(
            the_decision(&env, &parked.eid).await["request_sequence"],
            sequence,
            "{}",
            env.name
        );
    }
}

/// The other way round: a request appended through the backend that shows
/// options on a node whose workflow declares none. An approval naming one of
/// them is refused, and so is a rejection.
#[tokio::test]
async fn options_on_a_request_for_a_node_that_declares_none_is_409() {
    for env in envs().await {
        let parked = park(&env, &ir(None, None)).await;
        let sequence = append_request(&env, &parked, |c| c["options"] = topics()).await;
        options_refused_then_plain_decided(&env, &parked, sequence).await;
    }
}

/// A run whose workflow version is not in the store declares no options, so
/// a request on it that shows options is refused. One that shows none is
/// decided, as before options existed.
#[tokio::test]
async fn options_on_a_request_of_a_run_whose_version_is_missing_is_409() {
    for env in envs().await {
        // The initiator and the request context as the API writes them.
        let model = park(&env, &ir(None, None)).await;
        let initiated_by = events(&env, &model.eid)
            .await
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::WorkflowStarted { initiated_by, .. } => initiated_by,
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: the run records who started it", env.name));

        let eid = ExecutionId::new();
        let now = Utc::now();
        env.backend
            .create_execution(WorkflowExecution {
                execution_id: eid.clone(),
                workflow_id: "wf-gone".into(),
                workflow_version: "0.1.0".into(),
                status: WorkflowStatus::Running,
                initial_input: json!({}),
                current_state: json!({}),
                started_at: now,
                updated_at: now,
                completed_at: None,
                session_type: None,
                parent_execution_id: None,
                segment_number: 0,
            })
            .await
            .unwrap();
        env.backend
            .append_event(Event::new(
                eid.clone(),
                1,
                EventKind::WorkflowStarted {
                    workflow_id: "wf-gone".into(),
                    workflow_version: "0.1.0".into(),
                    initial_input: json!({}),
                    initiated_by: Some(initiated_by),
                },
            ))
            .await
            .unwrap();
        assert!(
            env.backend
                .get_workflow("wf-gone", "0.1.0")
                .await
                .unwrap()
                .is_none(),
            "{}: the version is not in the store",
            env.name
        );

        let parked = Parked {
            eid,
            context: model.context.clone(),
            digest: model.digest.clone(),
        };
        let sequence = append_request(&env, &parked, |c| c["options"] = topics()).await;
        options_refused_then_plain_decided(&env, &parked, sequence).await;
    }
}

/// A request written before options existed: its context has none, and its
/// node declares none, so an approval needs no choice and takes none.
#[tokio::test]
async fn a_request_from_before_options_needs_no_choice() {
    const DIGEST: &str = "5d41402abc4b2a76b9719d911017c592ae41ab1a5d41402abc4b2a76b9719d91";
    for env in envs().await {
        let eid = start(&env, &ir(None, None)).await;
        let seq = env.backend.latest_sequence(&eid).await.unwrap() + 1;
        // A request as an older build wrote it, word for word.
        env.backend
            .append_event(Event::new(
                eid.clone(),
                seq,
                serde_json::from_value(json!({
                    "type": "tool_approval_required",
                    "node_id": "pick_topic",
                    "tool_name": "pick_topic",
                    "approver": "human",
                    "context": {
                        "node_id": "pick_topic",
                        "description": "Which topic goes out this week?",
                        "action_digest": DIGEST,
                        "action_step": 0
                    }
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
        let parked = Parked {
            eid,
            context: Value::Null,
            digest: DIGEST.to_string(),
        };
        let answer = decide(
            &env,
            &parked,
            approval(DIGEST, json!({ "choice": "topic_1" })),
        )
        .await;
        assert_refused(&env, answer, StatusCode::BAD_REQUEST, NO_OPTIONS);

        let (st, body) = decide(
            &env,
            &parked,
            approval(DIGEST, json!({ "note": "As before." })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["approved_digest"], DIGEST, "{}", env.name);
        assert_eq!(decision["comment"], "As before.", "{}", env.name);
        assert!(decision.get("choice").is_none(), "{}: {decision}", env.name);
    }
}

/// The deadline comes before the choice: past it, a bad choice or none is
/// 409 expired, whether or not the sweep has recorded the expiry.
#[tokio::test]
async fn an_expired_request_with_a_bad_choice_is_409_expired() {
    const EXPIRED: &str = "the approval request for node 'pick_topic' expired at its deadline \
         and can no longer be decided; it was rejected by system:deadline";
    for env in envs().await {
        // Past `expires_at`, not swept yet.
        let parked = park(&env, &ir(Some(topics()), Some(60))).await;
        let past = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let sequence = append_request(&env, &parked, |c| c["expires_at"] = json!(past)).await;
        for extra in [
            json!({ "choice": "Topic_2" }),
            json!({}),
            json!({ "choice": "topic_2", "note": "\u{0}" }),
        ] {
            let mut body = approval(&parked.digest, extra.clone());
            body["request_sequence"] = json!(sequence);
            let answer = decide(&env, &parked, body).await;
            if extra.get("note").is_some() {
                // The note is checked first of all: it needs nothing read.
                assert_refused(&env, answer, StatusCode::BAD_REQUEST, BAD_CHARACTER);
            } else {
                assert_refused(&env, answer, StatusCode::CONFLICT, EXPIRED);
            }
        }

        // Swept: the expiry is on record.
        let parked = park(&env, &ir(Some(topics()), Some(60))).await;
        let expires_at: DateTime<Utc> = parked.context["expires_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        env.scheduler
            .sweep_approval_deadlines(&parked.eid, expires_at)
            .await
            .unwrap();
        for extra in [json!({ "choice": "Topic_2" }), json!({})] {
            let answer = decide(&env, &parked, approval(&parked.digest, extra)).await;
            assert_refused(&env, answer, StatusCode::CONFLICT, EXPIRED);
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn a_repeat_with_another_choice_is_409() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let (st, body) = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_2" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = events(&env, &parked.eid).await.len();
        for extra in [
            json!({ "choice": "topic_1" }),
            json!({ "choice": "Topic_2" }),
            json!({}),
        ] {
            let answer = decide(&env, &parked, approval(&parked.digest, extra)).await;
            assert_refused(
                &env,
                answer,
                StatusCode::CONFLICT,
                "already decided by you with a different decision",
            );
        }
        assert_eq!(
            events(&env, &parked.eid).await.len(),
            before,
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn a_repeat_with_the_same_choice_is_200() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let first = approval(
            &parked.digest,
            json!({ "choice": "topic_2", "note": "Go with the second one." }),
        );
        let (st, answer) = decide(&env, &parked, first.clone()).await;
        assert_eq!(st, StatusCode::OK, "{}: {answer}", env.name);
        assert_eq!(answer["repeat"], false, "{}", env.name);
        let sequence = answer["sequence"].clone();
        let before = events(&env, &parked.eid).await.len();

        // The same body, and the same note under its old name.
        let old_name = approval(
            &parked.digest,
            json!({ "choice": "topic_2", "comment": "Go with the second one." }),
        );
        for again in [first, old_name] {
            let (st, answer) = decide(&env, &parked, again).await;
            assert_eq!(st, StatusCode::OK, "{}: {answer}", env.name);
            assert_eq!(answer["repeat"], true, "{}: {answer}", env.name);
            assert_eq!(answer["sequence"], sequence, "{}", env.name);
        }
        assert_eq!(
            events(&env, &parked.eid).await.len(),
            before,
            "{}",
            env.name
        );
    }
}

#[tokio::test]
async fn the_mcp_approve_tool_takes_choice_and_note() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let tool = |extra: Value| {
            let mut args = json!({
                "execution_id": format!("exec_{}", parked.eid.0.simple()),
                "decision": "approved",
                "node_id": "pick_topic",
                "action_digest": parked.digest,
            });
            for (k, v) in extra.as_object().unwrap() {
                args[k] = v.clone();
            }
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "jamjet_approve", "arguments": args },
            })
        };
        let refused = [
            (json!({}), CHOICE_REQUIRED.to_string()),
            (
                json!({ "choice": "Topic_2" }),
                CHOICE_NOT_OFFERED.to_string(),
            ),
            (
                json!({ "choice": "topic_2", "note": "Fine\u{202e}" }),
                BAD_CHARACTER.to_string(),
            ),
            (
                json!({ "choice": "topic_2", "comment": "x".repeat(4097) }),
                "the note is 4097 bytes; a note is at most 4096 bytes".to_string(),
            ),
            (
                json!({ "choice": "topic_2", "note": "Ship it.", "comment": "Hold it." }),
                NOTE_AND_COMMENT.to_string(),
            ),
            (
                json!({ "choice": 2 }),
                "choice must be a string, not 2".to_string(),
            ),
            (
                json!({ "choice": "topic_2", "note": ["Ship it."] }),
                "note must be a string, not [\"Ship it.\"]".to_string(),
            ),
            (
                json!({ "choice": "topic_2", "comment": 7 }),
                "comment must be a string, not 7".to_string(),
            ),
        ];
        for (extra, error) in refused {
            let (st, resp) = call(&env, "/mcp", &env.bob, tool(extra.clone())).await;
            assert_eq!(st, StatusCode::OK, "{}: {resp}", env.name);
            assert_eq!(
                resp["result"]["isError"], true,
                "{}: {extra}: {resp}",
                env.name
            );
            assert_eq!(
                resp["result"]["content"][0]["text"], error,
                "{}: {extra}",
                env.name
            );
        }
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        let (st, resp) = call(
            &env,
            "/mcp",
            &env.bob,
            tool(json!({ "choice": "topic_2", "note": "From the tool." })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {resp}", env.name);
        assert_eq!(resp["result"]["isError"], false, "{}: {resp}", env.name);
        let decision = the_decision(&env, &parked.eid).await;
        assert_eq!(decision["choice"], "topic_2", "{}: {decision}", env.name);
        assert_eq!(decision["comment"], "From the tool.", "{}", env.name);

        // The tool says what it takes.
        let (_, listed) = call(
            &env,
            "/mcp",
            &env.bob,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
        let approve = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "jamjet_approve")
            .cloned()
            .unwrap_or_else(|| panic!("{}: {listed}", env.name));
        for field in ["choice", "note", "comment"] {
            assert_eq!(
                approve["inputSchema"]["properties"][field]["type"], "string",
                "{}: {field}",
                env.name
            );
        }
    }
}

/// A version stored before options were checked can hold options that break
/// the rule (an older engine dropped them when it read the version). The
/// node is refused: a reviewer is never shown them.
#[tokio::test]
async fn stored_options_that_break_the_rule_refuse_the_node() {
    let eleven: Vec<Value> = (1..=11)
        .map(|i| json!({ "id": format!("topic_{i}"), "label": format!("Topic {i}") }))
        .collect();
    let bidi = json!([{ "id": "topic_1", "label": "Fine \u{202e}txt.exe" }]);
    let upper = json!([{ "id": "Topic_1", "label": "Topic 1" }]);
    let cases = [
        (
            json!(eleven),
            "it declares 11 options; at most 10 are allowed",
        ),
        (
            bidi,
            "option 'topic_1': a label may not hold a control or invisible format character",
        ),
        (
            upper,
            "option id \"Topic_1\" is not allowed: an option id is 1 to 32 characters of a-z, \
             0-9, '_' or '-'",
        ),
    ];
    for (options, cause) in cases {
        for env in envs().await {
            let ir = ir(Some(options.clone()), None);
            support::store_ir(env.backend.as_ref(), &ir).await;
            let (st, body) = call(
                &env,
                "/executions",
                &env.alice,
                json!({ "workflow_id": WORKFLOW, "workflow_version": "0.1.0", "input": {} }),
            )
            .await;
            assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
            let id = body["execution_id"].as_str().unwrap();
            let eid =
                ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
            settle(&env).await;
            let evs = events(&env, &eid).await;
            assert!(
                !evs.iter()
                    .any(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. })),
                "{}: {options}: no request: {evs:#?}",
                env.name
            );
            let violation = evs
                .iter()
                .find_map(|e| match &e.kind {
                    EventKind::PolicyViolation { node_id, rule, .. } if node_id == "pick_topic" => {
                        Some(rule.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{}: {options}: a violation: {evs:#?}", env.name));
            assert_eq!(
                violation,
                format!(
                    "the node's options break the rule for options ({cause}); a reviewer is \
                     never shown them, so the node is refused"
                ),
                "{}",
                env.name
            );
            assert!(
                evs.iter()
                    .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })),
                "{}: {options}: the run fails: {evs:#?}",
                env.name
            );
        }
    }
}

// ── The choice in state, in the audit, in the route ────────────────────────

/// A node of the test workflows.
fn node(id: &str, kind: Value) -> Value {
    json!({ "id": id, "kind": kind, "retry_policy": null, "node_timeout_secs": null,
            "description": null, "labels": {} })
}

/// A `python_fn` step, which an external worker claims.
fn step(id: &str) -> Value {
    node(
        id,
        json!({ "type": "python_fn", "module": "myapp.posts", "function": id, "output_schema": "" }),
    )
}

/// `pick_topic` offers `topics()`; then each of `steps` runs in turn; then
/// `route` reads the choice: `revise` goes to `revise_list`, either topic to
/// `write_post`. No branch is taken without a choice.
fn routed_ir(workflow_id: &str, steps: &[&str]) -> Value {
    let on = |choice: &str, target: &str| {
        json!({
            "condition": format!("state.__approvals.pick_topic.choice == \"{choice}\""),
            "target": target
        })
    };
    let mut nodes = serde_json::Map::new();
    nodes.insert(
        "pick_topic".into(),
        node(
            "pick_topic",
            json!({ "type": "human_approval", "description": "Which topic goes out this week?",
                    "timeout_secs": null, "fallback_node": null, "options": topics() }),
        ),
    );
    for s in steps {
        nodes.insert(s.to_string(), step(s));
    }
    nodes.insert(
        "route".into(),
        node(
            "route",
            json!({ "type": "condition", "branches": [
                on("revise", "revise_list"),
                on("topic_1", "write_post"),
                on("topic_2", "write_post")
            ] }),
        ),
    );
    nodes.insert("revise_list".into(), step("revise_list"));
    nodes.insert("write_post".into(), step("write_post"));
    let mut chain = vec!["pick_topic"];
    chain.extend_from_slice(steps);
    chain.push("route");
    let mut edges: Vec<(&str, &str)> = chain.windows(2).map(|w| (w[0], w[1])).collect();
    edges.extend([
        ("route", "revise_list"),
        ("route", "write_post"),
        ("revise_list", "end"),
        ("write_post", "end"),
    ]);
    let edges: Vec<Value> = edges
        .into_iter()
        .map(|(from, to)| json!({ "from": from, "to": to, "condition": null }))
        .collect();
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pick_topic", "nodes": nodes, "edges": edges,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

async fn get(env: &Env, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Claim the next `python_fn` step over HTTP, as an external worker does.
async fn claim_step(env: &Env) -> Value {
    let (st, body) = call(
        env,
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "ext", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
    assert_eq!(body["claimed"], true, "{}: a step waits: {body}", env.name);
    body["work_item"].clone()
}

/// Report a claimed step done over HTTP, with `state_patch`.
async fn complete_step(env: &Env, item: &Value, state_patch: Value) {
    let (st, body) = call(
        env,
        &format!("/work-items/{}/complete", item["id"].as_str().unwrap()),
        &env.alice,
        json!({ "output": {}, "state_patch": state_patch, "lease_fence": item["lease_fence"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
}

/// The state patch the run's log records for `node`.
fn recorded_patch(events: &[Event], node: &str) -> Value {
    events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::NodeCompleted {
                node_id,
                state_patch,
                ..
            } if node_id == node => Some(state_patch.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{node} completed: {events:#?}"))
}

/// The run completed through `reached`, with `skipped` skipped. Returns its
/// final state.
async fn ended_through(env: &Env, eid: &ExecutionId, reached: &str, skipped: &str) -> Value {
    let evs = events(env, eid).await;
    let completed = |n: &str| {
        evs.iter()
            .any(|e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == n))
    };
    assert!(completed(reached), "{}: {reached} ran: {evs:#?}", env.name);
    assert!(!completed(skipped), "{}: {skipped} ran: {evs:#?}", env.name);
    assert!(
        evs.iter().any(
            |e| matches!(&e.kind, EventKind::NodeSkipped { node_id, .. } if node_id == skipped)
        ),
        "{}: {skipped} is skipped: {evs:#?}",
        env.name
    );
    evs.iter()
        .find_map(|e| match &e.kind {
            EventKind::WorkflowCompleted { final_state } => Some(final_state.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{}: the run completes: {evs:#?}", env.name))
}

/// The keys of a JSON object, sorted.
fn keys_of(record: &Value) -> Vec<String> {
    let mut keys: Vec<String> = record
        .as_object()
        .unwrap_or_else(|| panic!("an object: {record}"))
        .keys()
        .cloned()
        .collect();
    keys.sort_unstable();
    keys
}

/// A condition reads the choice from the run's state: `revise` reaches the
/// end of the revise branch, `topic_2` the end of the other.
#[tokio::test]
async fn a_condition_on_the_choice_routes_the_run() {
    for env in envs().await {
        let ir = routed_ir("wf-route", &[]);
        for (choice, reached, skipped) in [
            ("revise", "revise_list", "write_post"),
            ("topic_2", "write_post", "revise_list"),
        ] {
            let parked = park(&env, &ir).await;
            let note = format!("Picked {choice}.");
            let (st, body) = decide(
                &env,
                &parked,
                approval(&parked.digest, json!({ "choice": choice, "note": note })),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
            settle(&env).await;

            let item = claim_step(&env).await;
            assert_eq!(item["node_id"], reached, "{}: {choice}: {item}", env.name);
            let record = &item["payload"]["input"]["__approvals"]["pick_topic"];
            assert_eq!(record["choice"], choice, "{}: {record}", env.name);
            assert_eq!(record["note"], note.as_str(), "{}: {record}", env.name);
            assert_eq!(record["comment"], note.as_str(), "{}: {record}", env.name);
            complete_step(&env, &item, json!({})).await;
            settle(&env).await;

            let state = ended_through(&env, &parked.eid, reached, skipped).await;
            assert_eq!(
                state["__approvals"]["pick_topic"]["choice"], choice,
                "{}: {state}",
                env.name
            );
            let output = events(&env, &parked.eid)
                .await
                .into_iter()
                .find_map(|e| match e.kind {
                    EventKind::NodeCompleted {
                        node_id, output, ..
                    } if node_id == "pick_topic" => Some(output),
                    _ => None,
                })
                .unwrap();
            assert_eq!(output["choice"], choice, "{}: {output}", env.name);
            assert_eq!(output["note"], note.as_str(), "{}: {output}", env.name);
        }
    }
}

/// The decision is sealed into the audit log whole: its choice and note are in
/// the entry, the chain verifies, and an edit of the stored choice breaks it.
#[tokio::test]
async fn the_choice_and_note_are_in_the_sealed_audit_entry() {
    let note = "Go with the second one.";
    for (env, sealed) in sealed_envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let (st, body) = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_2", "note": note })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let (sequence, logged) = events(&env, &parked.eid)
            .await
            .into_iter()
            .find_map(|e| match e.kind {
                EventKind::ApprovalReceived { .. } => {
                    Some((e.sequence, serde_json::to_value(&e.kind).unwrap()))
                }
                _ => None,
            })
            .unwrap();

        let entries = sealed.entries().await;
        let types: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            ["tool_approval_required", "approval_received"],
            "{}",
            env.name
        );
        let index = 1;
        let entry = &entries[index];
        assert_eq!(entry.sequence, sequence, "{}", env.name);
        assert_eq!(entry.raw_event["choice"], "topic_2", "{}", env.name);
        assert_eq!(entry.raw_event["comment"], note, "{}", env.name);
        assert_eq!(entry.raw_event, logged, "{}: sealed whole", env.name);
        assert_eq!(
            verify_chain(&entries, &sealed.signer),
            Ok(()),
            "{}",
            env.name
        );

        sealed.edit_stored_choice("revise").await;
        let edited = sealed.entries().await;
        assert_eq!(edited[index].raw_event["choice"], "revise", "{}", env.name);
        assert_eq!(
            verify_chain(&edited, &sealed.signer),
            Err(ChainError::HashMismatch { index }),
            "{}",
            env.name
        );
    }
}

/// Runs a `python_fn` step in process, reporting `state_patch`.
struct Reports(Value);

#[async_trait::async_trait]
impl NodeExecutor for Reports {
    async fn execute(&self, _item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        Ok(ExecutionResult {
            output: json!({}),
            state_patch: self.0.clone(),
            duration_ms: 0,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

/// A step between the approval and the route reports an approval record that
/// names another option. The record is dropped from its patch, over HTTP and
/// in process, and the route follows the choice on record.
#[tokio::test]
async fn a_worker_cannot_forge_the_choice_in_state() {
    let patch = json!({
        "draft": "A post on durable timers.",
        "__approvals": { "pick_topic": {
            "approved": true, "decision": "approved", "choice": "revise", "note": "Forged."
        } }
    });
    for env in envs().await {
        let ir = routed_ir("wf-forge", &["draft"]);
        for transport in ["http", "in process"] {
            let parked = park(&env, &ir).await;
            let (st, body) = decide(
                &env,
                &parked,
                approval(
                    &parked.digest,
                    json!({ "choice": "topic_2", "note": "Go with the second one." }),
                ),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
            settle(&env).await;

            if transport == "http" {
                let item = claim_step(&env).await;
                assert_eq!(item["node_id"], "draft", "{}: {item}", env.name);
                complete_step(&env, &item, patch.clone()).await;
            } else {
                let worker = Worker::new(
                    "forger".into(),
                    env.backend.clone(),
                    vec!["python_tool".into()],
                )
                .register_executor("python_fn", Arc::new(Reports(patch.clone())));
                assert!(
                    worker.poll_and_execute().await.unwrap(),
                    "{}: draft runs",
                    env.name
                );
            }
            // The step's own key is kept and the record is dropped. (The
            // in-process worker adds its `__budget` bookkeeping to the patch.)
            let recorded = recorded_patch(&events(&env, &parked.eid).await, "draft");
            assert_eq!(
                recorded["draft"], "A post on durable timers.",
                "{}: {transport}: {recorded}",
                env.name
            );
            assert!(
                recorded.get("__approvals").is_none(),
                "{}: {transport}: the record is dropped: {recorded}",
                env.name
            );
            settle(&env).await;

            let item = claim_step(&env).await;
            assert_eq!(
                item["node_id"], "write_post",
                "{}: {transport}: routed on a forged choice: {item}",
                env.name
            );
            let record = &item["payload"]["input"]["__approvals"]["pick_topic"];
            assert_eq!(record["choice"], "topic_2", "{}: {record}", env.name);
            assert_eq!(
                record["note"], "Go with the second one.",
                "{}: {record}",
                env.name
            );
            complete_step(&env, &item, json!({})).await;
            settle(&env).await;
            let state = ended_through(&env, &parked.eid, "write_post", "revise_list").await;
            assert_eq!(
                state["__approvals"]["pick_topic"]["choice"], "topic_2",
                "{}: {transport}: {state}",
                env.name
            );
        }
    }
}

/// Both listings show a waiting request's options: they hand out the
/// request's context whole. The per-run listing reads the run's log until the
/// projector has seen the request, and the read model after; both show them.
#[tokio::test]
async fn the_per_run_and_cross_run_listings_show_the_options() {
    for env in envs().await {
        let parked = park(&env, &ir(Some(topics()), None)).await;
        let run = format!("exec_{}", parked.eid.0.simple());
        let per_run = format!("/executions/{run}/approvals");

        for read in ["from the log", "from the read model"] {
            if read == "from the read model" {
                Projector::new(env.backend.clone()).tick().await.unwrap();
            }
            let (st, listed) = get(&env, &per_run, &env.bob).await;
            assert_eq!(st, StatusCode::OK, "{}: {listed}", env.name);
            let pending = listed["pending"].as_array().unwrap();
            assert_eq!(pending.len(), 1, "{}: {read}: {listed}", env.name);
            assert_eq!(pending[0]["node_id"], "pick_topic", "{}", env.name);
            assert_eq!(
                pending[0]["context"]["options"],
                topics(),
                "{}: {read}",
                env.name
            );
        }

        let (st, listed) = get(&env, "/approvals", &env.bob).await;
        assert_eq!(st, StatusCode::OK, "{}: {listed}", env.name);
        let rows = listed["approvals"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{}: {listed}", env.name);
        assert_eq!(rows[0]["execution_id"], run.as_str(), "{}", env.name);
        assert_eq!(rows[0]["node_id"], "pick_topic", "{}", env.name);
        assert_eq!(rows[0]["context"]["options"], topics(), "{}", env.name);
    }
}

/// An expiry routed to the fallback writes a record with no choice and no
/// note, and a late approval naming an option changes nothing.
#[tokio::test]
async fn an_expiry_fallback_record_has_no_choice() {
    const EXPIRED: &str = "the approval request for node 'pick_topic' expired at its deadline \
         and can no longer be decided; it was rejected by system:deadline";
    for env in envs().await {
        let mut ir = ir(Some(topics()), Some(60));
        ir["workflow_id"] = json!("wf-expiry");
        ir["nodes"]["pick_topic"]["kind"]["fallback_node"] = json!("tell_owner");
        ir["nodes"]["tell_owner"] = step("tell_owner");
        ir["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "from": "tell_owner", "to": "end", "condition": null }));
        let parked = park(&env, &ir).await;
        let expires_at: DateTime<Utc> = parked.context["expires_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        env.scheduler
            .sweep_approval_deadlines(&parked.eid, expires_at)
            .await
            .unwrap();
        settle(&env).await;

        let answer = decide(
            &env,
            &parked,
            approval(&parked.digest, json!({ "choice": "topic_2" })),
        )
        .await;
        assert_refused(&env, answer, StatusCode::CONFLICT, EXPIRED);
        assert!(
            decisions(&env, &parked.eid).await.is_empty(),
            "{}",
            env.name
        );

        let item = claim_step(&env).await;
        assert_eq!(item["node_id"], "tell_owner", "{}: {item}", env.name);
        let record = &item["payload"]["input"]["__approvals"]["pick_topic"];
        assert_eq!(
            keys_of(record),
            ["approved", "decided_at", "decision"],
            "{}: {record}",
            env.name
        );
        assert_eq!(record["approved"], false, "{}", env.name);
        assert_eq!(record["decision"], "expired", "{}", env.name);
        complete_step(&env, &item, json!({})).await;
        settle(&env).await;

        let state = ended_through(&env, &parked.eid, "tell_owner", "publish").await;
        assert_eq!(
            keys_of(&state["__approvals"]["pick_topic"]),
            ["approved", "decided_at", "decision"],
            "{}: {state}",
            env.name
        );
    }
}

/// Where an environment's audit entries go: a SQLite audit log of its own,
/// and the key that signs them.
struct Sealed {
    audit: Arc<SqliteAuditBackend>,
    url: String,
    signer: AuditSigner,
}

impl Sealed {
    /// Every stored entry, oldest first (the chain's order).
    async fn entries(&self) -> Vec<AuditLogEntry> {
        let mut query = AuditQuery::new();
        query.limit = 100;
        let mut entries = self.audit.query(&query).await.unwrap();
        entries.reverse();
        entries
    }

    /// Rewrite the stored decision's `choice` in the database, as someone
    /// with write access to it could.
    async fn edit_stored_choice(&self, choice: &str) {
        let pool = sqlx::SqlitePool::connect(&self.url).await.unwrap();
        let (raw,): (String,) = sqlx::query_as(
            "SELECT raw_event FROM audit_log WHERE event_type = 'approval_received'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut raw: Value = serde_json::from_str(&raw).unwrap();
        raw["choice"] = json!(choice);
        let done = sqlx::query(
            "UPDATE audit_log SET raw_event = ? WHERE event_type = 'approval_received'",
        )
        .bind(raw.to_string())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(done.rows_affected(), 1);
        pool.close().await;
    }
}

/// [`envs`] with the audit log on disk: the approve route seals each decision
/// into it and the engine each request it raises.
async fn sealed_envs() -> Vec<(Env, Sealed)> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "jamjet_choice_audit_{}.db",
            uuid::Uuid::new_v4().simple()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let audit = Arc::new(SqliteAuditBackend::open(&url).await.unwrap());
        audit.migrate().await.unwrap();
        let signer = AuditSigner::new(b"approval-choice-audit-key".to_vec());
        let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
        let mut state = support::make_state(tb.dyn_backend());
        state.enricher = Arc::new(AuditEnricher::with_signer(
            audit_dyn.clone(),
            signer.clone(),
        ));
        state.audit = audit_dyn;
        out.push((
            env_on(tb.name(), state, true).await,
            Sealed { audit, url, signer },
        ));
    }
    out
}
