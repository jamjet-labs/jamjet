//! What the scheduler's integration tests share: the stores, small
//! workflows, a run started the way `POST /executions` starts one, and the
//! test standing in for a worker.
#![allow(dead_code)]

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::{Event, EventKind, InMemoryBackend, SqliteBackend, DEFAULT_TENANT};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub const WORKFLOW_ID: &str = "wf";
pub const VERSION: &str = "0.1.0";
/// Every node these workflows have is dispatched to this queue.
pub const QUEUE: &str = "general";

/// A backend under test, kept concrete for the test helpers the trait does
/// not have.
#[derive(Clone)]
pub enum Store {
    Sqlite(Arc<SqliteBackend>),
    Memory(Arc<InMemoryBackend>),
}

impl Store {
    pub fn name(&self) -> &'static str {
        match self {
            Store::Sqlite(_) => "sqlite",
            Store::Memory(_) => "memory",
        }
    }

    pub fn backend(&self) -> Arc<dyn StateBackend> {
        match self {
            Store::Sqlite(b) => b.clone(),
            Store::Memory(b) => b.clone(),
        }
    }

    /// Backdate the item's lease so the next sweep finds it expired.
    pub async fn expire_lease(&self, item_id: Uuid) {
        match self {
            Store::Sqlite(b) => b.force_lease_expired_for_test(item_id).await.unwrap(),
            Store::Memory(b) => b.force_lease_expired_for_test(item_id),
        }
    }
}

/// The URL of a SQLite file no other test uses.
pub fn sqlite_file_url() -> String {
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_scheduler_{}.db", Uuid::new_v4().simple()));
    format!("sqlite://{}", path.display())
}

/// A pool on the SQLite file at `url`, migrated. Each call opens a pool of
/// its own, as a second engine process would.
pub async fn open_sqlite(url: &str) -> Arc<SqliteBackend> {
    Arc::new(
        SqliteBackend::open(url)
            .await
            .expect("open the SQLite file"),
    )
}

/// A fresh SQLite file and a fresh memory backend.
pub async fn stores() -> Vec<Store> {
    vec![
        Store::Sqlite(open_sqlite(&sqlite_file_url()).await),
        Store::Memory(Arc::new(InMemoryBackend::new())),
    ]
}

/// A node of `kind` (a JSON node kind) with the named retry policy.
pub fn node(id: &str, kind: Value, retry_policy: Option<&str>) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "retry_policy": retry_policy,
        "node_timeout_secs": null,
        "description": null,
        "labels": {}
    })
}

/// A plain step: a condition node with no branches, which routes nothing,
/// so the scheduler dispatches it off its edges like any other node.
pub fn step(id: &str) -> Value {
    node(id, json!({ "type": "condition", "branches": [] }), None)
}

/// A workflow of `nodes` starting at `start`, with `edges` as (from, to,
/// condition).
pub fn workflow(start: &str, nodes: Vec<Value>, edges: &[(&str, &str, Option<&str>)]) -> Value {
    let nodes: serde_json::Map<String, Value> = nodes
        .into_iter()
        .map(|n| (n["id"].as_str().unwrap().to_string(), n))
        .collect();
    let edges: Vec<Value> = edges
        .iter()
        .map(|(from, to, condition)| json!({ "from": from, "to": to, "condition": condition }))
        .collect();
    json!({
        "workflow_id": WORKFLOW_ID,
        "version": VERSION,
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": start,
        "nodes": nodes,
        "edges": edges,
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

/// `ids[0] -> ids[1] -> ...`, each a plain step.
pub fn linear(ids: &[&str]) -> Value {
    let edges: Vec<(&str, &str, Option<&str>)> =
        ids.windows(2).map(|w| (w[0], w[1], None)).collect();
    workflow(ids[0], ids.iter().map(|id| step(id)).collect(), &edges)
}

/// Store `ir` and start one run of it as `POST /executions` does: the run,
/// its `WorkflowStarted`, the start node's `NodeScheduled` and the start
/// node's item, in one write.
pub async fn start_run(backend: &dyn StateBackend, ir: Value) -> ExecutionId {
    let now = chrono::Utc::now();
    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: WORKFLOW_ID.into(),
            version: VERSION.into(),
            ir: ir.clone(),
            created_at: now,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    let parsed: jamjet_ir::WorkflowIr = serde_json::from_value(ir).unwrap();
    let start = parsed.start_node.clone();
    let start_def = &parsed.nodes[&start];
    let execution_id = ExecutionId::new();
    let execution = WorkflowExecution {
        execution_id: execution_id.clone(),
        workflow_id: WORKFLOW_ID.into(),
        workflow_version: VERSION.into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    };
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: start.clone(),
        queue_type: QUEUE.into(),
        payload: jamjet_scheduler::build_work_item_payload(
            &parsed,
            &start,
            start_def,
            &serde_json::Map::new(),
            WORKFLOW_ID,
            VERSION,
        ),
        attempt: 0,
        max_attempts: jamjet_scheduler::max_attempts_for(start_def.retry_policy.as_deref()),
        created_at: now,
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    backend
        .start_execution_atomic(
            execution,
            EventKind::WorkflowStarted {
                workflow_id: WORKFLOW_ID.into(),
                workflow_version: VERSION.into(),
                initial_input: json!({}),
                initiated_by: None,
            },
            EventKind::NodeScheduled {
                node_id: start,
                queue_type: QUEUE.into(),
            },
            item,
        )
        .await
        .unwrap();
    execution_id
}

pub fn node_completed(node_id: &str, output: Value) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node_id.into(),
        output,
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

/// The test standing in for a worker. It claims every item that is pending
/// when it runs and remembers each item it ever held, by node. It runs after
/// every tick, so an item is pending for at most one tick and the items it
/// saw are the items the engine created.
#[derive(Default)]
pub struct Worker {
    seen: Mutex<BTreeMap<String, BTreeSet<Uuid>>>,
}

impl Worker {
    /// Claim every pending item, leaving each leased.
    pub async fn claim_all(&self, backend: &dyn StateBackend) -> Vec<WorkItem> {
        let mut claimed = Vec::new();
        while let Some(item) = backend
            .claim_work_item("test-worker", &[QUEUE])
            .await
            .unwrap()
        {
            self.seen
                .lock()
                .unwrap()
                .entry(item.node_id.clone())
                .or_default()
                .insert(item.id);
            claimed.push(item);
        }
        claimed
    }

    /// Claim every pending item and complete it under its lease, with the
    /// output `output_for` gives its node.
    pub async fn complete_all(
        &self,
        backend: &dyn StateBackend,
        output_for: impl Fn(&str) -> Value,
    ) {
        for item in self.claim_all(backend).await {
            backend
                .commit_turn(
                    item.id,
                    item.lease_fence,
                    Event::new(
                        item.execution_id.clone(),
                        0,
                        node_completed(&item.node_id, output_for(&item.node_id)),
                    ),
                    false,
                )
                .await
                .unwrap();
        }
    }

    /// The items the worker held for `node`.
    pub fn items_of(&self, node: &str) -> BTreeSet<Uuid> {
        self.seen
            .lock()
            .unwrap()
            .get(node)
            .cloned()
            .unwrap_or_default()
    }

    /// How many distinct items the worker held for each node.
    pub fn items_per_node(&self) -> BTreeMap<String, usize> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(node, ids)| (node.clone(), ids.len()))
            .collect()
    }
}

/// How many events `pick` names each node in.
pub fn per_node(
    events: &[Event],
    pick: impl Fn(&EventKind) -> Option<&String>,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        if let Some(node) = pick(&event.kind) {
            *counts.entry(node.clone()).or_insert(0) += 1;
        }
    }
    counts
}

pub fn scheduled(kind: &EventKind) -> Option<&String> {
    match kind {
        EventKind::NodeScheduled { node_id, .. } => Some(node_id),
        _ => None,
    }
}

/// `{node: 1}` for each of `nodes`.
pub fn once_each(nodes: &[&str]) -> BTreeMap<String, usize> {
    nodes.iter().map(|n| (n.to_string(), 1)).collect()
}

pub async fn status(backend: &dyn StateBackend, execution_id: &ExecutionId) -> WorkflowStatus {
    backend
        .get_execution(execution_id)
        .await
        .unwrap()
        .unwrap()
        .status
}

/// The kinds in the run's log, in order, for a failure message.
pub fn kinds(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            let json = serde_json::to_value(&e.kind).unwrap_or_default();
            match (json["type"].as_str(), json["node_id"].as_str()) {
                (Some(t), Some(n)) => format!("{}:{t}({n})", e.sequence),
                (Some(t), None) => format!("{}:{t}", e.sequence),
                _ => format!("{}:?", e.sequence),
            }
        })
        .collect()
}
