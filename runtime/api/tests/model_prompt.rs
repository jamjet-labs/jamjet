//! A model node's `prompt:` reaches the model, rendered against the run's state.
//!
//! Until 2026-11 nothing read `prompt_ref`: the work item carried only the
//! conversation in `state.messages`, so the YAML hello world (`jamjet init`'s
//! hello-agent: one model node whose prompt reads `{{ state.query }}`) failed
//! with "ModelNodeExecutor: no prompt or messages provided". These tests run
//! that workflow through registration, `POST /executions`, the scheduler and
//! an in-process worker with the real `ModelNodeExecutor` over a fake model.

mod support;

use jamjet_models::adapter::ChatRole;
use jamjet_models::{
    ModelAdapter, ModelError, ModelRegistry, ModelRequest, ModelResponse, StructuredRequest,
};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// A model that answers "Paris." and records every request it is sent.
struct RecordingModel {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

#[async_trait::async_trait]
impl ModelAdapter for RecordingModel {
    fn system_name(&self) -> &'static str {
        "recording"
    }
    fn default_model(&self) -> &str {
        "fake-model"
    }
    async fn chat(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(req);
        Ok(ModelResponse {
            content: "Paris.".into(),
            model: "fake-model".into(),
            finish_reason: "stop".into(),
            input_tokens: 1,
            output_tokens: 1,
            structured: None,
            tool_calls: vec![],
        })
    }
    async fn structured_output(&self, _: StructuredRequest) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }
}

fn hello_agent_ir() -> Value {
    serde_json::from_str(include_str!("fixtures/hello_agent_ir.json")).unwrap()
}

/// Start the hello world with `input`, then run the scheduler and one model
/// worker until the run is terminal. Returns the run's events and the
/// requests the model received.
async fn run_hello_world(input: Value) -> (Vec<jamjet_state::Event>, Vec<ModelRequest>) {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone());
    let eid = support::register_and_start(&state, hello_agent_ir(), input).await;
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap(),
    );

    let requests = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(
        ModelRegistry::new()
            .register(Arc::new(RecordingModel {
                requests: requests.clone(),
            }))
            .with_default("recording"),
    );
    let worker =
        jamjet_worker::Worker::new("model-w".into(), backend.clone(), vec!["model".into()])
            .register_executor(
                "model",
                Arc::new(jamjet_worker::ModelNodeExecutor::new(registry)),
            );
    let scheduler = Scheduler::new(backend.clone());

    for _ in 0..10 {
        let _ = worker.poll_and_execute().await;
        scheduler.tick().await.expect("scheduler tick");
        let events = backend.get_events(&eid).await.unwrap();
        if events.iter().any(|e| {
            matches!(
                e.kind,
                EventKind::WorkflowCompleted { .. } | EventKind::WorkflowFailed { .. }
            )
        }) {
            let reqs = std::mem::take(&mut *requests.lock().unwrap());
            return (events, reqs);
        }
    }
    panic!("the run did not reach a terminal state");
}

#[tokio::test]
async fn the_hello_world_prompt_reaches_the_model_with_the_state_filled_in() {
    let (events, requests) = run_hello_world(json!({ "query": "What is JamJet?" })).await;

    assert_eq!(requests.len(), 1, "one model call");
    let msgs = &requests[0].messages;
    assert_eq!(msgs.len(), 1, "the prompt is the single message: {msgs:?}");
    assert!(matches!(msgs[0].role, ChatRole::User));
    assert_eq!(
        msgs[0].content,
        "Answer this question clearly and concisely:\n\nWhat is JamJet?\n"
    );

    let final_state = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::WorkflowCompleted { final_state } => Some(final_state.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the run must complete: {:#?}", events));
    assert_eq!(final_state["last_model_output"], "Paris.");
}

/// A prompt naming state the run does not have fails the node with that
/// reason. The model is never called with a prompt that has a hole in it.
#[tokio::test]
async fn a_prompt_naming_state_the_run_lacks_fails_the_node_without_a_model_call() {
    let (events, requests) =
        run_hello_world(json!({ "question": "What is the capital of France?" })).await;

    assert!(requests.is_empty(), "the model must not be called");
    let error = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::NodeFailed {
                node_id,
                error,
                retryable: false,
                ..
            } if node_id == "think" => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the node must fail terminally: {:#?}", events));
    assert!(error.contains("no `query`"), "{error}");
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })));
}

/// Syntax the runtime cannot render is refused at registration, naming the
/// node, instead of failing every run that reaches it.
#[tokio::test]
async fn a_prompt_the_runtime_cannot_render_is_refused_at_registration() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend);
    for prompt in [
        "Results: {{ state.results | join(', ') }}",
        "{%- if state.critique %}Revise.{% endif %}",
    ] {
        let mut ir = hello_agent_ir();
        ir["nodes"]["think"]["kind"]["prompt_ref"] = json!(prompt);
        let (st, body) = support::post(&state, "/workflows", json!({ "ir": ir })).await;
        assert_eq!(st.as_u16(), 400, "{prompt}: {body}");
        let msg = body.to_string();
        assert!(msg.contains("think"), "names the node: {msg}");
    }
}
