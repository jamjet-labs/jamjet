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

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use jamjet_worker::Worker;
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
        let backend = tb.dyn_backend();
        let app = build_router_with_opts(support::make_state(backend.clone()), false);
        let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
        let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
        let engine = jamjet_worker::server_pool(
            backend.clone(),
            Arc::new(jamjet_models::ModelRegistry::new()),
        )
        .build_worker("engine".into(), vec!["general".into()]);
        out.push(Env {
            name: tb.name(),
            scheduler: Scheduler::new(backend.clone()),
            backend,
            app,
            alice,
            bob,
            engine,
        });
    }
    out
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
/// Returns the new request's sequence.
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
        .unwrap();
    seq
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
