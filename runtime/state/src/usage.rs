//! A run's usage, folded from its own event log: the tokens and the cost of
//! every step that spent, across every segment of the run.
//!
//! Only `NodeCompleted` counts. A completion is billable when it names a
//! model or reports tokens or a cost. Its cost is usable when
//! [`cost_to_nanos`] takes it; a stored cost that is not (a negative one in
//! an old log, a non-finite one built in memory, one above
//! [`MAX_REPORTED_COST_USD`]) counts as unknown. Its tokens are unknown when
//! it reports neither count. The caps read these counts
//! (`crate::caps`): an unknown cost under a dollar cap, or unknown tokens
//! under a token cap, stops the run.
//!
//! Costs are summed as integer nano-dollars, so that $0.10 and $0.20 reach a
//! $0.30 cap exactly. Summed as `f64` they land at 0.30000000000000004 and
//! would read as past it.

use std::collections::HashSet;
use std::fmt;

use jamjet_core::workflow::ExecutionId;
use serde::Serialize;

use crate::backend::{BackendResult, StateBackend};
use crate::event::{Event, EventKind};
use crate::segment::segment_chain;

/// The largest cost one step may report, in USD.
pub const MAX_REPORTED_COST_USD: f64 = 10_000.0;

/// A cost in nano-dollars (1 = 1e-9 USD). `Some` only for a finite cost in
/// `0..=MAX_REPORTED_COST_USD`.
pub fn cost_to_nanos(c: f64) -> Option<u64> {
    (c.is_finite() && (0.0..=MAX_REPORTED_COST_USD).contains(&c)).then(|| (c * 1e9).round() as u64)
}

/// What a run has spent, from its log.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunUsage {
    /// Input tokens of the billable steps (saturating sum).
    pub input_tokens: u64,
    /// Output tokens of the billable steps (saturating sum).
    pub output_tokens: u64,
    /// The usable costs, in nano-dollars (saturating sum).
    #[serde(skip)]
    pub cost_nanos: u64,
    /// Steps that named a model or reported tokens or a cost.
    pub billable_steps: u64,
    /// Billable steps with no usable cost.
    pub cost_unknown_steps: u64,
    /// Billable steps that reported no tokens at all.
    pub tokens_unknown_steps: u64,
    /// The node of the last billable step folded.
    #[serde(skip)]
    pub last_billable_node: Option<String>,
    /// Idempotency keys of the billable steps counted, so a replayed
    /// completion is counted once.
    #[serde(skip)]
    counted_keys: HashSet<String>,
}

/// The log holds an event this build cannot read (`EventKind::Unknown`). It
/// may be a spend, so the usage cannot be known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the run's log holds an event this build cannot read")
    }
}

impl std::error::Error for Unreadable {}

impl RunUsage {
    /// Fold one event. Only `NodeCompleted` counts. A completion whose
    /// idempotency_key was already counted is skipped (a replay).
    /// `EventKind::Unknown` is `Err(Unreadable)`.
    pub fn apply(&mut self, e: &Event) -> Result<(), Unreadable> {
        let EventKind::NodeCompleted {
            node_id,
            gen_ai_model,
            input_tokens,
            output_tokens,
            cost_usd,
            idempotency_key,
            ..
        } = &e.kind
        else {
            return match e.kind {
                EventKind::Unknown => Err(Unreadable),
                _ => Ok(()),
            };
        };
        let billable = gen_ai_model.is_some()
            || input_tokens.is_some()
            || output_tokens.is_some()
            || cost_usd.is_some();
        if !billable {
            return Ok(());
        }
        if let Some(key) = idempotency_key {
            if !self.counted_keys.insert(key.clone()) {
                return Ok(());
            }
        }
        self.billable_steps = self.billable_steps.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(output_tokens.unwrap_or(0));
        if input_tokens.is_none() && output_tokens.is_none() {
            self.tokens_unknown_steps = self.tokens_unknown_steps.saturating_add(1);
        }
        match cost_usd.and_then(cost_to_nanos) {
            Some(nanos) => self.cost_nanos = self.cost_nanos.saturating_add(nanos),
            None => self.cost_unknown_steps = self.cost_unknown_steps.saturating_add(1),
        }
        self.last_billable_node = Some(node_id.clone());
        Ok(())
    }

    /// Input and output tokens together (saturating).
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// The cost in USD. For display only: the caps compare nano-dollars.
    pub fn cost_usd(&self) -> f64 {
        self.cost_nanos as f64 / 1e9
    }

    /// This usage and a later segment's together: the sums (saturating),
    /// the keys each counted, and the later segment's last billable node
    /// when it has one. A key counted in both is counted twice; keys are
    /// derived from the segment's own id, so two segments share none.
    pub fn plus(&self, later: &RunUsage) -> RunUsage {
        RunUsage {
            input_tokens: self.input_tokens.saturating_add(later.input_tokens),
            output_tokens: self.output_tokens.saturating_add(later.output_tokens),
            cost_nanos: self.cost_nanos.saturating_add(later.cost_nanos),
            billable_steps: self.billable_steps.saturating_add(later.billable_steps),
            cost_unknown_steps: self
                .cost_unknown_steps
                .saturating_add(later.cost_unknown_steps),
            tokens_unknown_steps: self
                .tokens_unknown_steps
                .saturating_add(later.tokens_unknown_steps),
            last_billable_node: later
                .last_billable_node
                .clone()
                .or_else(|| self.last_billable_node.clone()),
            counted_keys: self
                .counted_keys
                .union(&later.counted_keys)
                .cloned()
                .collect(),
        }
    }
}

/// Every segment of the run, root first (`segment_chain`), folded with
/// [`RunUsage::apply`]. `id` may be any segment: the fold covers it and the
/// segments before it. `Ok(Err(Unreadable))` when a segment's log holds an
/// event this build cannot read.
pub async fn fold_run_usage(
    b: &dyn StateBackend,
    id: &ExecutionId,
) -> BackendResult<Result<RunUsage, Unreadable>> {
    let mut usage = RunUsage::default();
    for segment in segment_chain(b, id).await? {
        for e in b.get_events(&segment).await? {
            if let Err(unreadable) = usage.apply(&e) {
                return Ok(Err(unreadable));
            }
        }
    }
    Ok(Ok(usage))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::StateBackendError;
    use crate::memory::InMemoryBackend;
    use crate::sqlite::SqliteBackend;
    use crate::tenant::{Tenant, TenantId, TenantStatus};
    use chrono::Utc;
    use jamjet_core::workflow::{WorkflowExecution, WorkflowStatus};
    use serde_json::json;
    use std::sync::Arc;

    /// A step's completion with the telemetry given, and no model named.
    pub(crate) fn completion(
        node: &str,
        input: Option<u64>,
        output: Option<u64>,
        cost: Option<f64>,
        key: Option<&str>,
    ) -> Event {
        Event::new(
            ExecutionId::new(),
            1,
            EventKind::NodeCompleted {
                node_id: node.into(),
                output: json!({}),
                state_patch: json!({}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: input,
                output_tokens: output,
                finish_reason: None,
                cost_usd: cost,
                provenance: None,
                idempotency_key: key.map(String::from),
            },
        )
    }

    pub(crate) fn fold(events: &[Event]) -> RunUsage {
        let mut u = RunUsage::default();
        for e in events {
            u.apply(e).expect("a readable event");
        }
        u
    }

    #[test]
    fn a_zero_cost_is_known() {
        let u = fold(&[completion("m", Some(10), Some(5), Some(0.0), None)]);
        assert_eq!(
            (u.billable_steps, u.cost_unknown_steps, u.cost_nanos),
            (1, 0, 0)
        );
        // A cost of zero and nothing else is still a billable step with a
        // known cost; its tokens are the unknown part.
        let u = fold(&[completion("m", None, None, Some(0.0), None)]);
        assert_eq!(
            (
                u.billable_steps,
                u.cost_unknown_steps,
                u.tokens_unknown_steps,
                u.cost_nanos
            ),
            (1, 0, 1, 0)
        );
    }

    #[test]
    fn a_negative_cost_in_an_old_log_counts_as_unknown() {
        // The complete route refuses a negative cost; a log written before it
        // did can hold one.
        let old: EventKind = serde_json::from_str(
            r#"{"type":"node_completed","node_id":"m","output":{},"state_patch":{},"duration_ms":1,"input_tokens":10,"output_tokens":5,"cost_usd":-0.5}"#,
        )
        .unwrap();
        let u = fold(&[Event::new(ExecutionId::new(), 3, old)]);
        assert_eq!(
            (u.billable_steps, u.cost_unknown_steps, u.cost_nanos),
            (1, 1, 0)
        );
        assert_eq!(u.total_tokens(), 15);
        assert_eq!(cost_to_nanos(-0.5), None);
        assert_eq!(cost_to_nanos(-1e-12), None);
    }

    #[test]
    fn a_non_finite_cost_counts_as_unknown() {
        // JSON cannot carry these; an executor can build them in memory.
        for c in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let u = fold(&[completion("m", Some(1), Some(1), Some(c), None)]);
            assert_eq!(
                (u.billable_steps, u.cost_unknown_steps, u.cost_nanos),
                (1, 1, 0),
                "{c}"
            );
            assert_eq!(cost_to_nanos(c), None, "{c}");
        }
    }

    #[test]
    fn a_cost_above_the_per_call_ceiling_counts_as_unknown() {
        assert_eq!(
            cost_to_nanos(MAX_REPORTED_COST_USD),
            Some(10_000_000_000_000)
        );
        let at = fold(&[completion("m", Some(1), Some(1), Some(10_000.0), None)]);
        assert_eq!(
            (at.cost_unknown_steps, at.cost_nanos),
            (0, 10_000_000_000_000)
        );
        let above = fold(&[completion("m", Some(1), Some(1), Some(10_000.01), None)]);
        assert_eq!((above.cost_unknown_steps, above.cost_nanos), (1, 0));
        assert_eq!(cost_to_nanos(10_000.000_001), None);
    }

    #[test]
    fn a_replayed_completion_with_the_same_key_counts_once() {
        let u = fold(&[
            completion("m", Some(100), Some(20), Some(0.5), Some("k1")),
            completion("m", Some(100), Some(20), Some(0.5), Some("k1")),
        ]);
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cost_nanos,
                u.billable_steps
            ),
            (100, 20, 500_000_000, 1)
        );
        // Another key is another step.
        let u = fold(&[
            completion("m", Some(100), Some(20), Some(0.5), Some("k1")),
            completion("m", Some(100), Some(20), Some(0.5), Some("k2")),
        ]);
        assert_eq!((u.input_tokens, u.billable_steps), (200, 2));
    }

    #[test]
    fn two_completions_without_keys_both_count() {
        let u = fold(&[
            completion("m", Some(100), Some(20), Some(0.5), None),
            completion("m", Some(100), Some(20), Some(0.5), None),
        ]);
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cost_nanos,
                u.billable_steps
            ),
            (200, 40, 1_000_000_000, 2)
        );
    }

    #[test]
    fn a_completion_that_reports_nothing_is_not_billable() {
        let u = fold(&[completion("condition", None, None, None, Some("k"))]);
        assert_eq!(u, RunUsage::default());

        // Naming a model is enough to be billable, with both counts unknown.
        let mut named = completion("m", None, None, None, None);
        if let EventKind::NodeCompleted { gen_ai_model, .. } = &mut named.kind {
            *gen_ai_model = Some("model-1".into());
        }
        let u = fold(&[named]);
        assert_eq!(
            (
                u.billable_steps,
                u.cost_unknown_steps,
                u.tokens_unknown_steps
            ),
            (1, 1, 1)
        );
        assert_eq!(u.last_billable_node.as_deref(), Some("m"));
    }

    #[test]
    fn token_sums_saturate() {
        let u = fold(&[
            completion("m", Some(u64::MAX), Some(u64::MAX), None, None),
            completion("m", Some(1), Some(1), None, None),
        ]);
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.total_tokens()),
            (u64::MAX, u64::MAX, u64::MAX)
        );
        let both = u.plus(&u);
        assert_eq!(
            (both.input_tokens, both.total_tokens(), both.billable_steps),
            (u64::MAX, u64::MAX, 4)
        );
    }

    #[test]
    fn plus_adds_a_later_segment_and_keeps_both_keys() {
        let earlier = fold(&[completion("a", Some(10), Some(1), Some(0.25), Some("k1"))]);
        let later = fold(&[completion("b", None, None, Some(0.5), Some("k2"))]);
        let mut both = earlier.plus(&later);
        assert_eq!(
            (
                both.input_tokens,
                both.output_tokens,
                both.cost_nanos,
                both.billable_steps,
                both.tokens_unknown_steps
            ),
            (10, 1, 750_000_000, 2, 1)
        );
        assert_eq!(both.last_billable_node.as_deref(), Some("b"));
        // A later segment without a billable step keeps the earlier node.
        assert_eq!(
            earlier
                .plus(&RunUsage::default())
                .last_billable_node
                .as_deref(),
            Some("a")
        );
        // Both keys are counted: a replay of either is skipped.
        both.apply(&completion("a", Some(10), Some(1), Some(0.25), Some("k1")))
            .unwrap();
        both.apply(&completion("b", None, None, Some(0.5), Some("k2")))
            .unwrap();
        assert_eq!(both.billable_steps, 2);
    }

    // ── Folding a run's log ───────────────────────────────────────────────

    async fn register_tenant(db: &SqliteBackend, id: &str) {
        let now = Utc::now();
        db.for_tenant(TenantId::default())
            .create_tenant(Tenant {
                id: TenantId::from(id),
                name: id.to_string(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }

    /// Memory, SQLite, and SQLite scoped to a tenant other than the
    /// default. The scoped store comes with its database, to look at the
    /// run from another tenant.
    async fn stores() -> Vec<(&'static str, Arc<dyn StateBackend>, Option<SqliteBackend>)> {
        let scoped = SqliteBackend::open("sqlite::memory:").await.unwrap();
        register_tenant(&scoped, "acme").await;
        vec![
            ("memory", Arc::new(InMemoryBackend::new()), None),
            (
                "sqlite",
                Arc::new(SqliteBackend::open("sqlite::memory:").await.unwrap()),
                None,
            ),
            (
                "sqlite scoped",
                Arc::new(scoped.for_tenant(TenantId::from("acme"))),
                Some(scoped),
            ),
        ]
    }

    async fn segment(
        b: &dyn StateBackend,
        parent: Option<&ExecutionId>,
        number: u32,
    ) -> ExecutionId {
        let id = ExecutionId::new();
        let now = Utc::now();
        b.create_execution(WorkflowExecution {
            execution_id: id.clone(),
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: parent.cloned(),
            segment_number: number,
        })
        .await
        .unwrap();
        id
    }

    async fn append(b: &dyn StateBackend, id: &ExecutionId, mut e: Event) {
        let seq = b.latest_sequence(id).await.unwrap() + 1;
        e.execution_id = id.clone();
        e.sequence = seq;
        b.append_event(e).await.unwrap();
    }

    #[tokio::test]
    async fn usage_folds_across_segments() {
        for (name, b, db) in stores().await {
            let b = b.as_ref();
            let root = segment(b, None, 0).await;
            let seg1 = segment(b, Some(&root), 1).await;
            let seg2 = segment(b, Some(&seg1), 2).await;
            append(
                b,
                &root,
                completion("a", Some(100), Some(10), Some(0.25), Some("k-a")),
            )
            .await;
            append(
                b,
                &seg1,
                completion("b", Some(200), Some(20), Some(0.5), Some("k-b")),
            )
            .await;
            append(
                b,
                &seg2,
                completion("c", Some(400), Some(40), Some(0.125), Some("k-c")),
            )
            .await;

            let whole = fold_run_usage(b, &seg2).await.unwrap().unwrap();
            assert_eq!(
                (
                    whole.input_tokens,
                    whole.output_tokens,
                    whole.cost_nanos,
                    whole.billable_steps
                ),
                (700, 70, 875_000_000, 3),
                "{name}"
            );
            // Root first: the last billable step folded is the latest
            // segment's.
            assert_eq!(whole.last_billable_node.as_deref(), Some("c"), "{name}");

            // From a middle segment: it and the segments before it.
            let middle = fold_run_usage(b, &seg1).await.unwrap().unwrap();
            assert_eq!(
                (
                    middle.input_tokens,
                    middle.cost_nanos,
                    middle.billable_steps
                ),
                (300, 750_000_000, 2),
                "{name}"
            );
            assert_eq!(middle.last_billable_node.as_deref(), Some("b"), "{name}");

            let only_root = fold_run_usage(b, &root).await.unwrap().unwrap();
            assert_eq!(
                (only_root.input_tokens, only_root.billable_steps),
                (100, 1),
                "{name}"
            );

            // Another tenant's scope does not see the run.
            if let Some(db) = db {
                let other = db.for_tenant(TenantId::from("other"));
                match fold_run_usage(&other, &seg2).await {
                    Err(StateBackendError::NotFound(_)) => {}
                    other => panic!("{name}: another tenant folded the run: {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn an_unknown_event_makes_usage_unreadable() {
        let mut u = RunUsage::default();
        u.apply(&completion("m", Some(1), Some(1), Some(0.1), None))
            .unwrap();
        assert_eq!(
            u.apply(&Event::new(ExecutionId::new(), 2, EventKind::Unknown)),
            Err(Unreadable)
        );

        for (name, b, _) in stores().await {
            let b = b.as_ref();
            let root = segment(b, None, 0).await;
            let seg1 = segment(b, Some(&root), 1).await;
            append(b, &root, completion("a", Some(1), Some(1), Some(0.1), None)).await;
            append(b, &root, Event::new(root.clone(), 0, EventKind::Unknown)).await;
            append(b, &seg1, completion("b", Some(1), Some(1), Some(0.1), None)).await;
            // An unreadable event in an earlier segment makes the whole
            // run's usage unreadable.
            assert_eq!(
                fold_run_usage(b, &seg1).await.unwrap(),
                Err(Unreadable),
                "{name}"
            );
        }
    }
}
