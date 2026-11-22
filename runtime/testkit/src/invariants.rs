//! The rules a store holds on every path, read straight from its database,
//! for tests that run engines as processes, kill them or restart their
//! database.
//!
//! 1. Per run, sequences are 1..N with no gap or repeat.
//! 2. Per run and node, at most one `NodeCompleted` per dispatch occurrence,
//!    and never two live items for one node.
//! 3. Every running run has, for each node it holds as scheduled, a live
//!    item or an open approval request: nothing is stranded.
//! 4. Every finished run has exactly one terminal event, of its status, and
//!    no event after it but `AuditSealFailed`. A run that is not finished
//!    has none.
//! 5. Postgres only: every sealed event has exactly one audit entry, the
//!    chain verifies with one genesis, and the outbox is empty.
//!
//! [`read`] takes what the rules need from the database in one go, and
//! [`check_snapshot`] applies them; [`check`] does both. A run that rolled
//! over to a new segment (`limit_exceeded`) is left out of rule 4: its log
//! ends at the rollover, not at a terminal event.

use jamjet_audit::{verify_chain, AuditBackend, AuditLogEntry, AuditQuery, AuditSigner};
use serde_json::Value;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{PgPool, Row, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

/// The HMAC key the engines these rules check sign the audit chain with:
/// a harness starts every engine and command with it as
/// `JAMJET_AUDIT_SIGNING_KEY`, and rule 5 verifies the chain with it.
pub const AUDIT_KEY: &str = "jamjet-invariants-test-key";

/// Where a store keeps its rows.
#[derive(Debug, Clone)]
pub enum Store {
    /// A SQLite database file.
    SqliteFile(PathBuf),
    /// A Postgres database, by URL.
    Postgres(String),
}

/// One broken rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The rule's number, 1 to 5, as in the module docs.
    pub rule: u8,
    /// The run it is about, as the store writes the id; `None` for the
    /// audit chain and the outbox.
    pub run: Option<String>,
    /// What is wrong.
    pub what: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.run {
            Some(run) => write!(f, "rule {} in run {run}: {}", self.rule, self.what),
            None => write!(f, "rule {}: {}", self.rule, self.what),
        }
    }
}

/// A run as its row stands.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub status: String,
}

/// An event as its row stands: its kind as JSON, with its `type` tag.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub id: String,
    pub run: String,
    pub sequence: i64,
    pub kind: Value,
}

impl StoredEvent {
    /// The kind's `type` tag.
    pub fn kind_type(&self) -> &str {
        self.kind["type"].as_str().unwrap_or("")
    }

    /// The node the kind names, if it names one.
    pub fn node(&self) -> Option<&str> {
        self.kind["node_id"].as_str()
    }
}

/// What the Postgres audit tables hold.
#[derive(Debug, Clone)]
pub struct AuditState {
    /// Every entry, in chain order.
    pub entries: Vec<AuditLogEntry>,
    /// The event ids the outbox still holds.
    pub outbox: Vec<String>,
}

/// What the rules read, taken from one store.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub runs: Vec<Run>,
    /// Every event, by run and then by sequence.
    pub events: Vec<StoredEvent>,
    /// The `(run, node)` of every pending or claimed item, one per item.
    pub live_items: Vec<(String, String)>,
    /// `None` on SQLite, which has no outbox and is not checked by rule 5.
    pub audit: Option<AuditState>,
}

impl Snapshot {
    /// The events of `run`, in sequence order.
    pub fn events_of<'a>(&'a self, run: &'a str) -> impl Iterator<Item = &'a StoredEvent> + 'a {
        self.events.iter().filter(move |e| e.run == run)
    }

    /// The status of `run`, `None` if there is no such run.
    pub fn status_of(&self, run: &str) -> Option<&str> {
        self.runs
            .iter()
            .find(|r| r.id == run)
            .map(|r| r.status.as_str())
    }

    /// How many audit entries name `event_id`. Zero on SQLite, whose audit
    /// log this snapshot does not read.
    pub fn entries_for(&self, event_id: &str) -> usize {
        self.audit.as_ref().map_or(0, |a| {
            a.entries
                .iter()
                .filter(|e| e.event_id.to_string() == event_id)
                .count()
        })
    }
}

/// Every broken rule in `store`. Panics if the store cannot be read.
pub async fn check(store: &Store) -> Vec<Violation> {
    check_snapshot(&read(store).await)
}

/// Read what the rules need from `store`. Panics if it cannot be read.
pub async fn read(store: &Store) -> Snapshot {
    match store {
        Store::SqliteFile(path) => read_sqlite(path).await,
        Store::Postgres(url) => read_postgres(url).await,
    }
}

/// The event kinds that end a run, by the status they end it in.
const TERMINAL: &[(&str, &str)] = &[
    ("workflow_completed", "completed"),
    ("workflow_failed", "failed"),
    ("workflow_cancelled", "cancelled"),
];

/// The statuses of a finished run that rule 4 checks.
const FINISHED: &[&str] = &["completed", "failed", "cancelled"];

/// Apply rules 1 to 5 to `snapshot`.
pub fn check_snapshot(snapshot: &Snapshot) -> Vec<Violation> {
    let mut found = Vec::new();
    let mut by_run: BTreeMap<&str, Vec<&StoredEvent>> = BTreeMap::new();
    for event in &snapshot.events {
        by_run.entry(&event.run).or_default().push(event);
    }
    for events in by_run.values_mut() {
        events.sort_by_key(|e| e.sequence);
    }
    let mut live: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (run, node) in &snapshot.live_items {
        *live.entry((run, node)).or_default() += 1;
    }
    let none = Vec::new();
    for run in &snapshot.runs {
        let events = by_run.get(run.id.as_str()).unwrap_or(&none);
        dense(run, events, &mut found);
        once_per_dispatch(run, events, &mut found);
        if run.status == "running" {
            nothing_stranded(run, events, &live, &mut found);
        }
        one_end(run, events, &mut found);
    }
    for ((run, node), count) in &live {
        if *count > 1 {
            found.push(violation(
                2,
                Some(run),
                format!("{count} live items for node {node}"),
            ));
        }
    }
    if let Some(audit) = &snapshot.audit {
        sealed_once(snapshot, audit, &mut found);
    }
    found
}

fn violation(rule: u8, run: Option<&str>, what: String) -> Violation {
    Violation {
        rule,
        run: run.map(str::to_string),
        what,
    }
}

/// Rule 1: the run's sequences are 1..N.
fn dense(run: &Run, events: &[&StoredEvent], found: &mut Vec<Violation>) {
    for (i, event) in events.iter().enumerate() {
        let expected = i as i64 + 1;
        if event.sequence != expected {
            found.push(violation(
                1,
                Some(&run.id),
                format!(
                    "sequence {} where {expected} was due: the sequences are not 1..{}",
                    event.sequence,
                    events.len()
                ),
            ));
            return;
        }
    }
}

/// Rule 2, first half: one `NodeCompleted` per node per `NodeScheduled`.
fn once_per_dispatch(run: &Run, events: &[&StoredEvent], found: &mut Vec<Violation>) {
    let mut completions: BTreeMap<&str, u32> = BTreeMap::new();
    for event in events {
        let Some(node) = event.node() else { continue };
        match event.kind_type() {
            "node_scheduled" => {
                completions.insert(node, 0);
            }
            "node_completed" => {
                let n = completions.entry(node).or_default();
                *n += 1;
                if *n == 2 {
                    found.push(violation(
                        2,
                        Some(&run.id),
                        format!(
                            "node {node} completed twice for one dispatch (sequence {})",
                            event.sequence
                        ),
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Where the log leaves a node of a running run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Dispatched or due for a retry: a worker has, or will claim, its item.
    Scheduled,
    /// Waiting for a decision on an open approval request.
    ForApproval,
    /// Done, failed for good, or decided and waiting for the scheduler.
    Not,
}

/// Rule 3: each node the log holds as scheduled has a live item, or an
/// open approval request.
fn nothing_stranded(
    run: &Run,
    events: &[&StoredEvent],
    live: &BTreeMap<(&str, &str), usize>,
    found: &mut Vec<Violation>,
) {
    let mut held: BTreeMap<&str, Held> = BTreeMap::new();
    for event in events {
        let Some(node) = event.node() else { continue };
        let now = match event.kind_type() {
            "node_scheduled" | "retry_scheduled" => Held::Scheduled,
            "tool_approval_required" => Held::ForApproval,
            "node_completed" | "node_skipped" | "node_cancelled" => Held::Not,
            "approval_received" | "approval_expired" => Held::Not,
            // A retryable failure is followed by its retry in the same
            // write, and the item goes back to pending.
            "node_failed" if event.kind["retryable"] == Value::Bool(true) => continue,
            "node_failed" => Held::Not,
            _ => continue,
        };
        held.insert(node, now);
    }
    for (node, state) in held {
        if state == Held::Scheduled && !live.contains_key(&(run.id.as_str(), node)) {
            found.push(violation(
                3,
                Some(&run.id),
                format!("node {node} is scheduled and has no live item"),
            ));
        }
    }
}

/// Rule 4: one terminal event, of the run's status, and nothing after it
/// but `AuditSealFailed`; none in a run that is not finished.
fn one_end(run: &Run, events: &[&StoredEvent], found: &mut Vec<Violation>) {
    let ends: Vec<&&StoredEvent> = events
        .iter()
        .filter(|e| TERMINAL.iter().any(|(t, _)| *t == e.kind_type()))
        .collect();
    if !FINISHED.contains(&run.status.as_str()) {
        if run.status != "limit_exceeded" && !ends.is_empty() {
            found.push(violation(
                4,
                Some(&run.id),
                format!(
                    "a {} run has a terminal event ({})",
                    run.status,
                    ends[0].kind_type()
                ),
            ));
        }
        return;
    }
    let [end] = ends.as_slice() else {
        found.push(violation(
            4,
            Some(&run.id),
            format!(
                "a {} run has {} terminal events, not one",
                run.status,
                ends.len()
            ),
        ));
        return;
    };
    let ends_as = TERMINAL
        .iter()
        .find(|(t, _)| *t == end.kind_type())
        .map(|(_, s)| *s);
    if ends_as != Some(run.status.as_str()) {
        found.push(violation(
            4,
            Some(&run.id),
            format!("a {} run ends with {}", run.status, end.kind_type()),
        ));
    }
    for later in events.iter().filter(|e| e.sequence > end.sequence) {
        if later.kind_type() != "audit_seal_failed" {
            found.push(violation(
                4,
                Some(&run.id),
                format!(
                    "{} at sequence {} after the terminal event",
                    later.kind_type(),
                    later.sequence
                ),
            ));
        }
    }
}

/// Rule 5: each sealed event is in the chain once, the chain verifies from
/// one genesis, and the outbox is empty.
fn sealed_once(snapshot: &Snapshot, audit: &AuditState, found: &mut Vec<Violation>) {
    let sealed = jamjet_state::approvals::SEALED_EVENT_KINDS;
    let mut per_event: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &audit.entries {
        *per_event.entry(entry.event_id.to_string()).or_default() += 1;
    }
    for event in &snapshot.events {
        if !sealed.contains(&event.kind_type()) {
            continue;
        }
        let n = per_event.get(&event.id).copied().unwrap_or(0);
        if n != 1 {
            found.push(violation(
                5,
                Some(&event.run),
                format!(
                    "{} at sequence {} has {n} audit entries, not one",
                    event.kind_type(),
                    event.sequence
                ),
            ));
        }
    }
    if !audit.entries.is_empty() {
        let genesis = audit
            .entries
            .iter()
            .filter(|e| e.prev_hash.is_none())
            .count();
        if genesis != 1 {
            found.push(violation(
                5,
                None,
                format!("the chain has {genesis} entries with no predecessor, not one"),
            ));
        }
        if let Err(e) = verify_chain(&audit.entries, &AuditSigner::new(AUDIT_KEY)) {
            found.push(violation(
                5,
                None,
                format!("the chain does not verify: {e}"),
            ));
        }
    }
    let waiting: BTreeSet<&String> = audit.outbox.iter().collect();
    if !waiting.is_empty() {
        found.push(violation(
            5,
            None,
            format!(
                "the outbox still holds {} events: {waiting:?}",
                waiting.len()
            ),
        ));
    }
}

async fn read_sqlite(path: &std::path::Path) -> Snapshot {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePool::connect_with(options)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let runs = sqlx::query("SELECT execution_id, status FROM workflow_executions")
        .fetch_all(&pool)
        .await
        .expect("read the runs")
        .iter()
        .map(|r| Run {
            id: r.get(0),
            status: r.get(1),
        })
        .collect();
    let events = sqlx::query(
        "SELECT id, execution_id, sequence, kind_json FROM events \
         ORDER BY execution_id, sequence",
    )
    .fetch_all(&pool)
    .await
    .expect("read the events")
    .iter()
    .map(|r| event(r.get(0), r.get(1), r.get(2), r.get(3)))
    .collect();
    let live_items = sqlx::query(
        "SELECT execution_id, node_id FROM work_items WHERE status IN ('pending', 'claimed')",
    )
    .fetch_all(&pool)
    .await
    .expect("read the live items")
    .iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();
    pool.close().await;
    Snapshot {
        runs,
        events,
        live_items,
        audit: None,
    }
}

async fn read_postgres(url: &str) -> Snapshot {
    let pool = PgPool::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connect to the store: {e}"));
    let runs = sqlx::query("SELECT execution_id::text, status FROM workflow_executions")
        .fetch_all(&pool)
        .await
        .expect("read the runs")
        .iter()
        .map(|r| Run {
            id: r.get(0),
            status: r.get(1),
        })
        .collect();
    let events = sqlx::query(
        "SELECT id::text, execution_id::text, sequence, kind_json FROM events \
         ORDER BY execution_id, sequence",
    )
    .fetch_all(&pool)
    .await
    .expect("read the events")
    .iter()
    .map(|r| event(r.get(0), r.get(1), r.get(2), r.get(3)))
    .collect();
    let live_items = sqlx::query(
        "SELECT execution_id::text, node_id FROM work_items \
         WHERE status IN ('pending', 'claimed')",
    )
    .fetch_all(&pool)
    .await
    .expect("read the live items")
    .iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect();
    // Newest first, as the audit routes list it; the chain is oldest first.
    let mut entries = jamjet_audit::PgAuditBackend::new(pool.clone())
        .query(&AuditQuery {
            limit: u32::MAX,
            ..AuditQuery::default()
        })
        .await
        .expect("read the audit log");
    entries.reverse();
    let outbox = sqlx::query_scalar("SELECT event_id::text FROM audit_outbox")
        .fetch_all(&pool)
        .await
        .expect("read the outbox");
    pool.close().await;
    Snapshot {
        runs,
        events,
        live_items,
        audit: Some(AuditState { entries, outbox }),
    }
}

fn event(id: String, run: String, sequence: i64, kind_json: String) -> StoredEvent {
    let kind = serde_json::from_str(&kind_json)
        .unwrap_or_else(|e| panic!("event {id} of run {run} is not JSON: {e}"));
    StoredEvent {
        id,
        run,
        sequence,
        kind,
    }
}
