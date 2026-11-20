//! A caller's `run_key` on `POST /executions`: the rule a key follows, the
//! hash of the input it binds, and the segment of its run that is running
//! now.
//!
//! A key names one run per tenant. The same key with the same workflow,
//! version and input answers with that run, whatever its status. Another
//! workflow, version or input under the key is refused, for good. With
//! `reuse: "failed_only"` a new run starts once the latest segment of the
//! run the key names ended without success, and the key names the new run
//! from then on.

use crate::error::ApiError;
use axum::http::StatusCode;
use axum::Json;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{RunKeyClaim, RunKeyRecord, StateBackend};
use jamjet_state::EventKind;
use serde_json::{json, Value};

/// The longest key, in bytes.
pub const MAX_RUN_KEY_BYTES: usize = 200;

/// The most segments followed from a run's first one. A chain is acyclic;
/// past this the log is corrupt.
const MAX_SEGMENTS: usize = 10_000;

/// Whether a key may be given to a new run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reuse {
    /// Never: the key names one run for good. The default.
    Never,
    /// Once the run the key names ended without success.
    FailedOnly,
}

/// A start's key and reuse rule, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartKey {
    pub run_key: String,
    pub reuse: Reuse,
}

impl StartKey {
    /// The key and reuse a start request sent. A key outside the rule, a
    /// `reuse` other than `never` or `failed_only`, and a `reuse` without a
    /// key are 400.
    pub fn from_request(
        run_key: Option<String>,
        reuse: Option<String>,
    ) -> Result<Option<Self>, ApiError> {
        let Some(run_key) = run_key else {
            return match reuse {
                None => Ok(None),
                Some(_) => Err(ApiError::BadRequest(
                    "reuse applies to a start with a run_key only".into(),
                )),
            };
        };
        check_run_key(&run_key)?;
        let reuse = match reuse.as_deref() {
            None | Some("never") => Reuse::Never,
            Some("failed_only") => Reuse::FailedOnly,
            Some(_) => {
                return Err(ApiError::BadRequest(
                    "reuse must be never or failed_only".into(),
                ))
            }
        };
        Ok(Some(Self { run_key, reuse }))
    }
}

/// A key is 1 to [`MAX_RUN_KEY_BYTES`] bytes of `A-Z a-z 0-9 . _ : / @ + = -`.
/// It is checked as sent: never trimmed, case-folded or normalised, so two
/// keys are the same key only when their bytes are.
pub fn check_run_key(run_key: &str) -> Result<(), ApiError> {
    let fits = (1..=MAX_RUN_KEY_BYTES).contains(&run_key.len());
    if fits && run_key.bytes().all(is_key_byte) {
        return Ok(());
    }
    Err(ApiError::BadRequest(format!(
        "run_key must be 1 to {MAX_RUN_KEY_BYTES} bytes of A-Z, a-z, 0-9 and . _ : / @ + = -"
    )))
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'+' | b'=' | b'-')
}

/// The input a key binds: the content hash of its canonical JSON, so key
/// order does not matter. `input` is the run's input as `start_run` stores
/// it, the engine's own keys already dropped (`action_state`).
pub fn input_hash(input: &Value) -> String {
    jamjet_state::content_hash(input)
}

/// A start of another workflow, version or input under a held key is 409,
/// in every mode and for good.
pub fn check_binding(claim: &RunKeyClaim, held: &RunKeyRecord) -> Result<(), ApiError> {
    let same = held.workflow_id == claim.workflow_id
        && held.workflow_version == claim.workflow_version
        && held.input_hash == claim.input_hash;
    if same {
        return Ok(());
    }
    Err(ApiError::Conflict(format!(
        "run_key '{}' names a run of another workflow, version or input",
        claim.run_key
    )))
}

/// The answer to a start that made a new run under `run_key`.
pub fn created_answer(run_key: &str, execution_id: &ExecutionId) -> (StatusCode, Json<Value>) {
    (
        StatusCode::CREATED,
        Json(json!({
            "execution_id": execution_id.to_string(),
            "run_key": run_key,
            "created": true,
        })),
    )
}

/// The run a key names: its first segment, which the key holds, and the
/// segment running now (the same one until the run rolls over).
#[derive(Debug, Clone)]
pub struct KeyedRun {
    pub record: RunKeyRecord,
    pub first: WorkflowExecution,
    pub latest: WorkflowExecution,
}

impl KeyedRun {
    /// Read the run `record` names, following each segment's
    /// `SegmentBoundary.next_execution_id` to the latest one.
    ///
    /// Fails closed on a log holding an event this build cannot read: it could
    /// be a later release's link to a next segment, so which segment runs now
    /// is not known.
    pub async fn read(backend: &dyn StateBackend, record: RunKeyRecord) -> Result<Self, ApiError> {
        let first = segment(backend, &record.execution_id, None).await?;
        let mut latest = first.clone();
        for _ in 0..MAX_SEGMENTS {
            match next_segment(backend, &latest.execution_id).await? {
                None => {
                    return Ok(Self {
                        record,
                        first,
                        latest,
                    })
                }
                Some(next) => latest = segment(backend, &next, Some(&latest.execution_id)).await?,
            }
        }
        Err(ApiError::Internal(format!(
            "run {} has more than {MAX_SEGMENTS} segments",
            record.execution_id
        )))
    }

    /// Whether `reuse` gives the key to a new run: `failed_only`, and the
    /// run's latest segment failed, was cancelled, or hit a limit with no
    /// next segment. The first segment's status does not say: it ends
    /// `limit_exceeded` when the run rolls over.
    pub fn may_restart(&self, reuse: Reuse) -> bool {
        reuse == Reuse::FailedOnly
            && matches!(
                self.latest.status,
                WorkflowStatus::Failed | WorkflowStatus::Cancelled | WorkflowStatus::LimitExceeded
            )
    }

    /// The answer to a start that found this run under `run_key`.
    pub fn start_answer(&self, run_key: &str) -> (StatusCode, Json<Value>) {
        (
            StatusCode::OK,
            Json(json!({
                "execution_id": self.record.execution_id.to_string(),
                "current_execution_id": self.latest.execution_id.to_string(),
                "run_key": run_key,
                "created": false,
                "status": self.latest.status,
            })),
        )
    }

    /// The run as `GET /executions?run_key=` lists it: the first segment's
    /// row as stored, with the segment running now and its status.
    fn lookup_row(&self) -> Result<Value, ApiError> {
        let mut row = serde_json::to_value(&self.first)
            .map_err(|e| ApiError::Internal(format!("serialize execution: {e}")))?;
        row["current_execution_id"] = json!(self.latest.execution_id.to_string());
        row["current_status"] = json!(self.latest.status);
        Ok(row)
    }
}

/// `GET /executions?run_key=`: the run the key names in this tenant, or none.
/// The key is echoed, so a caller can tell this answer from an older
/// engine's, which ignores the query and lists every run.
pub async fn lookup(backend: &dyn StateBackend, run_key: &str) -> Result<Value, ApiError> {
    check_run_key(run_key)?;
    let executions = match backend.get_run_key(run_key).await? {
        None => Vec::new(),
        Some(record) => vec![KeyedRun::read(backend, record).await?.lookup_row()?],
    };
    Ok(json!({ "run_key": run_key, "executions": executions }))
}

/// The execution `id`, which must be the next segment of `parent` when one is
/// given.
async fn segment(
    backend: &dyn StateBackend,
    id: &ExecutionId,
    parent: Option<&ExecutionId>,
) -> Result<WorkflowExecution, ApiError> {
    let execution = backend.get_execution(id).await?.ok_or_else(|| {
        ApiError::Internal(format!("execution {id} of a keyed run is not stored"))
    })?;
    if parent.is_some() && execution.parent_execution_id.as_ref() != parent {
        return Err(ApiError::Internal(format!(
            "execution {id} is linked as a next segment but is not one"
        )));
    }
    Ok(execution)
}

/// The segment that `id`'s `SegmentBoundary` names, if it rolled over.
async fn next_segment(
    backend: &dyn StateBackend,
    id: &ExecutionId,
) -> Result<Option<ExecutionId>, ApiError> {
    let mut next = None;
    for event in backend.get_events(id).await? {
        match event.kind {
            EventKind::SegmentBoundary {
                next_execution_id, ..
            } if next.is_none() => next = Some(next_execution_id),
            EventKind::Unknown => {
                return Err(ApiError::Internal(format!(
                    "the log of {id} holds an event this build cannot read, so the segment \
                     running now is not known; upgrade the engine"
                )))
            }
            _ => {}
        }
    }
    next.map(|s| {
        uuid::Uuid::parse_str(s.strip_prefix("exec_").unwrap_or(&s))
            .map(ExecutionId)
            .map_err(|_| ApiError::Internal(format!("{id} links to a bad segment id {s:?}")))
    })
    .transpose()
}
