//! Validated approval submission, shared by the REST route and the MCP bridge.
//!
//! All approval decisions pass through `submit_approval` which folds the event log
//! to verify something is actually pending before appending `ApprovalReceived`.
//! The listing helper `approvals_view` builds the pending/decided payload for
//! `GET /executions/:id/approvals`, and `pending_row_view` one row of
//! `GET /approvals`.
//!
//! A decision sent again is answered with the first one and records nothing.
//! It repeats the decision D on record for node N only when all hold:
//! 1. D answers N's latest request (an expiry is not a decision, and a
//!    decision that does not name its request is never repeated);
//! 2. D was made by the same principal (in dev mode: neither has one and the
//!    recorded user is the same);
//! 3. the same decision, the same choice, and the same note (absent, empty or
//!    blank is none);
//! 4. its echo passes the rule a new decision meets, against the request D
//!    answered: missing on an approval outside dev mode is 400, a different
//!    one is 409 stale, and a `request_sequence` that names another request
//!    is 409;
//! 5. with no `node_id`, exactly one node's decision matches and no request
//!    is pending. A match beside a pending request could be either a retry
//!    or a decision on that request, so it is 400 "specify node_id".
//!
//! Anything else on a decided request is 409 `AlreadyDecided`.
//!
//! A node can ask again with the same digest: a step released past an
//! agent's limit that fails asks again for its retry. A retry without
//! `request_sequence` then matches the new request and decides it. A retry
//! that names the request it answered can only answer that one.
//!
//! A request can offer options (a `human_approval` node's, listed in its
//! context under `options`). Approving it needs a `choice`, one of their
//! ids, byte for byte. A rejection, or a decision on a request without
//! options, takes no choice. The options a request shows must be the ones
//! the node declares in the workflow version the run is bound to: anything
//! else is a doctored log or a bug, and nothing is recorded.
//!
//! Every decision can carry a note (`comment` is its old name): absent,
//! empty or blank is no note; otherwise at most [`MAX_NOTE_BYTES`] bytes,
//! with no control character but newline, carriage return and tab, and no
//! invisible format character. It is checked before anything is read.

use jamjet_core::node::ApprovalOption;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::approvals::{
    answered_request, expired_by_deadline, node_approval_status, pending_approvals,
    pending_but_expired, run_initiator, still_open, NodeApprovalStatus, PendingApproval,
    ESCALATE_AT_KEY, EXPIRES_AT_KEY,
};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::{ApprovalCursor, Event, PendingApprovalRow};
use std::sync::Arc;

pub struct ApprovalSubmission {
    pub node_id: Option<String>,
    /// Display name recorded as the approver.
    pub user_id: String,
    /// Authenticated identity (`"token:<id>"`), or `None` in dev mode. Callers
    /// derive it from the transport's auth, never from request content.
    pub principal: Option<String>,
    pub decision: ApprovalDecision,
    /// The id of the option the decider picked. Required to approve a request
    /// that offers options, refused anywhere else.
    pub choice: Option<String>,
    /// The decider's note, as sent: a transport reads it with [`note_from`],
    /// which takes `comment` as its old name. Checked first of all, and
    /// recorded as the decision's `comment`.
    pub note: Option<String>,
    /// Always refused when non-null: see [`SubmitError::EditsUnsupported`].
    /// Carried so both transports refuse it in one place rather than one of
    /// them dropping it silently.
    pub state_patch: Option<serde_json::Value>,
    /// The `action_digest` the decider was shown, echoed back. When present it
    /// must equal the pending request's digest, or the decision is refused as
    /// stale (the request it was made against has been superseded).
    pub action_digest: Option<String>,
    /// The sequence of the request the decider answers, echoed back. When
    /// present it must be the sequence of the request this decision would
    /// answer, or the decision is refused with [`SubmitError::StaleRequest`].
    pub request_sequence: Option<i64>,
    /// The transport runs without authentication (dev mode). Outside it, an
    /// `Approved` decision must echo the request's binding.
    pub dev_mode: bool,
}

/// Lets the principal that started a run approve it.
pub const ALLOW_SELF_APPROVAL_ENV: &str = "JAMJET_ALLOW_SELF_APPROVAL";

/// Install-wide approval rules, read once at startup into `AppState`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPolicy {
    /// Let the principal that started a run approve its actions. Off by
    /// default; `JAMJET_ALLOW_SELF_APPROVAL=1` turns it on for single-operator
    /// installs where there is nobody else to approve.
    pub allow_self_approval: bool,
}

impl ApprovalPolicy {
    /// Read [`ALLOW_SELF_APPROVAL_ENV`], a boolean switch
    /// ([`jamjet_core::flag`]). `jamjet-server` refuses a value that is not a
    /// boolean before it gets here; a library caller reads it as off, which
    /// enforces separation of duties.
    pub fn from_env() -> Self {
        Self {
            allow_self_approval: jamjet_core::flag::env_flag(ALLOW_SELF_APPROVAL_ENV)
                .unwrap_or(false),
        }
    }
}

#[derive(Debug)]
pub enum SubmitError {
    /// Nothing is waiting for approval on this execution.
    NoPending,
    /// `node_id` omitted but several nodes are pending; caller must disambiguate.
    MultiplePending(Vec<String>),
    /// The named node has no outstanding approval request.
    NodeNotPending(String),
    /// The execution is already terminal; a decision can no longer change it.
    ExecutionTerminal(String),
    /// The approving principal started this run (separation of duties).
    SelfApproval,
    /// Outside dev mode, the run has no recorded initiator, so separation of
    /// duties cannot be proven for an approval.
    InitiatorUnknown,
    /// The echoed `action_digest` is not the pending request's digest.
    StaleDigest,
    /// The echoed `request_sequence` is not the request this decision would
    /// answer: the node has asked again since, or it names another request.
    StaleRequest,
    /// Outside dev mode, an `Approved` decision on a request that binds an
    /// action did not echo the binding it was made against.
    DigestRequired,
    /// Outside dev mode, the decision carries no authenticated principal. Both
    /// transports always supply one there, so this is a misconfigured caller:
    /// an unattributable decision is never recorded.
    PrincipalMissing,
    /// A decision carried a `state_patch`. Approval edits are not supported:
    /// the executor builds node payloads from the scheduler's own state, which
    /// never applies an approval's patch, so recording "approved as edited"
    /// would be a false record of what ran.
    EditsUnsupported,
    /// The request reached its deadline: it is expired (rejected by
    /// `system:deadline`) or past `expires_at` and about to be. Nothing is
    /// recorded.
    Expired(String),
    /// The log holds event kinds this build cannot read, so approval state
    /// cannot be determined. Nothing is recorded.
    UnreadableLog,
    /// The request was decided, expired or replaced between this decision's
    /// read and its append. Nothing is recorded.
    Superseded(String),
    /// The node's latest request already has a decision this one does not
    /// repeat: a different decision or note by the same principal
    /// (`by_caller`), or any decision by another. Nothing is recorded.
    AlreadyDecided {
        node: String,
        by_caller: bool,
    },
    /// An approval of a request that offers options named none. Holds the
    /// ids offered, in order.
    ChoiceRequired(Vec<String>),
    /// The choice is not one of the ids the request offers, compared byte
    /// for byte. Holds the ids offered; the caller's text is not echoed.
    ChoiceNotOffered(Vec<String>),
    /// A choice where none is taken: on a rejection, or on a request that
    /// offers no options. Holds the reason.
    ChoiceNotAllowed(&'static str),
    /// The options the request shows are not the ones the run's workflow
    /// declares for the node. Only a doctored log or a bug writes such a
    /// request, so nothing is recorded, whatever the choice.
    OptionsChanged,
    /// The note breaks the note rule (see the module docs), or `note` and
    /// `comment` were both sent and differ. Holds the reason.
    BadNote(String),
    Backend(String),
}

/// The longest note a decision may carry, in bytes of UTF-8.
pub const MAX_NOTE_BYTES: usize = 4096;

const REJECTION_TAKES_NO_CHOICE: &str = "a rejection takes no choice";
const NO_OPTIONS_TAKES_NO_CHOICE: &str =
    "this request offers no options, so a decision on it takes no choice";

/// What [`submit_approval`] recorded, or found on record.
#[derive(Debug)]
pub struct Submitted {
    /// The node the decision answers.
    pub node_id: String,
    /// The `ApprovalReceived` on record, with its sequence: the one just
    /// appended, or for a repeat the first one.
    pub event: Event,
    /// The same decision was already on record: nothing was appended, sealed
    /// or resumed.
    pub repeat: bool,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPending => write!(f, "no pending approval on this execution"),
            Self::MultiplePending(nodes) => write!(
                f,
                "multiple approvals pending ({}); specify node_id",
                nodes.join(", ")
            ),
            Self::NodeNotPending(n) => write!(
                f,
                "node '{n}' has no pending approval (unknown or already decided)"
            ),
            Self::ExecutionTerminal(status) => write!(
                f,
                "execution is {status}; approvals can no longer be decided"
            ),
            Self::SelfApproval => write!(
                f,
                "separation of duties: the principal that started this run cannot approve it"
            ),
            Self::InitiatorUnknown => write!(
                f,
                "separation of duties: this run has no recorded initiator (it predates \
                 initiator recording or was started in dev mode), so an approval cannot be \
                 shown to come from someone other than whoever started it; reject it, re-run \
                 it, or set JAMJET_ALLOW_SELF_APPROVAL=1 to allow approvals without that check"
            ),
            Self::StaleDigest => write!(
                f,
                "action_digest does not match the pending request; re-read the pending approval and decide again"
            ),
            Self::StaleRequest => write!(
                f,
                "request_sequence does not match the pending request; re-read the pending approvals and decide again"
            ),
            Self::DigestRequired => write!(
                f,
                "action_digest is required to approve: echo the action_digest (or, for an \
                 agent tool dispatch, the calls_hash) from the context of the pending \
                 approval you reviewed, as listed by GET /executions/{{id}}/approvals"
            ),
            Self::EditsUnsupported => write!(
                f,
                "approval edits are not supported: a decision cannot carry a state_patch, \
                 because the action would not run on the edited state; approve or reject \
                 the action as requested"
            ),
            Self::PrincipalMissing => write!(
                f,
                "the decision has no authenticated principal; outside dev mode every \
                 approval decision must come from an API token"
            ),
            Self::Expired(n) => write!(
                f,
                "the approval request for node '{n}' expired at its deadline and can no longer \
                 be decided; it was rejected by system:deadline"
            ),
            Self::UnreadableLog => {
                write!(f, "{}", jamjet_state::approvals::UNREADABLE_LOG_REASON)
            }
            Self::Superseded(n) => write!(
                f,
                "the approval request for node '{n}' was decided, expired or replaced while \
                 this decision was being made; nothing was recorded, re-read the pending \
                 approvals"
            ),
            Self::AlreadyDecided {
                by_caller: true, ..
            } => write!(f, "already decided by you with a different decision"),
            Self::AlreadyDecided {
                by_caller: false, ..
            } => write!(f, "already decided by another approver"),
            Self::ChoiceRequired(ids) => write!(
                f,
                "approving this request needs a choice, one of: {}",
                ids.join(", ")
            ),
            Self::ChoiceNotOffered(ids) => write!(
                f,
                "the choice is not one of this request's options: {}",
                ids.join(", ")
            ),
            Self::ChoiceNotAllowed(reason) => write!(f, "{reason}"),
            Self::OptionsChanged => write!(
                f,
                "the options in this request are not the ones the run's workflow declares \
                 for the node; nothing was recorded"
            ),
            Self::BadNote(reason) => write!(f, "{reason}"),
            Self::Backend(e) => write!(f, "backend error: {e}"),
        }
    }
}

/// Validate against derived pending state, then append `ApprovalReceived`.
///
/// Returns the resolved `node_id` and the `ApprovalReceived` event that was
/// appended, so callers can seal it into the signed, hash-chained audit log
/// (via `AuditEnricher::enrich_and_append`) after the durable event write.
///
/// A submission that repeats the decision on record (the rules in the module
/// docs) returns that decision with `repeat: true`. Nothing is appended and
/// the run is not resumed, so callers seal nothing for it. The repeat is
/// checked before the run's status, so a retry after the run ended still
/// gets its answer.
///
/// The note is checked before anything is read. The choice is checked after
/// the deadline and the echo, so a late or stale decision is refused as that
/// whatever its choice (the rules are in the module docs).
///
/// Separation of duties: an `Approved` decision whose principal is the run's
/// initiator (`WorkflowStarted.initiated_by`) is refused with
/// [`SubmitError::SelfApproval`] and nothing is appended, unless
/// `policy.allow_self_approval`. Rejections are never refused on this ground.
/// Outside dev mode, a run with no recorded initiator (dev-mode runs, runs
/// logged before initiators were recorded) is refused with
/// [`SubmitError::InitiatorUnknown`], since SoD cannot be proven for it. In
/// dev mode there is no principal and the check does not apply.
///
/// SoD distinguishes TOKENS, not people: see the note in the body.
pub async fn submit_approval(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    mut submission: ApprovalSubmission,
    policy: ApprovalPolicy,
) -> Result<Submitted, SubmitError> {
    // The note needs nothing read: checked first, and from here on it is
    // the note as recorded (a blank one is none).
    submission.note = checked_note(submission.note.take())?;

    // No edits, on either decision, before anything is read or written.
    if submission
        .state_patch
        .as_ref()
        .is_some_and(|p| !p.is_null())
    {
        return Err(SubmitError::EditsUnsupported);
    }

    let events = backend
        .get_events(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    // Fail closed on a log this build cannot fully read: an unreadable event
    // may have revoked or decided what the listing still shows as pending.
    if jamjet_state::approvals::has_unreadable_events(&events) {
        return Err(SubmitError::UnreadableLog);
    }

    // The same decision sent again: answer with the one on record.
    if let Some((node_id, event)) = repeated_decision(&events, &submission)? {
        return Ok(Submitted {
            node_id,
            event,
            repeat: true,
        });
    }

    // Refuse decisions on terminal executions up front. The event-derived
    // pending list can still show an undecided request after a cancellation
    // (the fold intentionally ignores decisions for unheld nodes), so without
    // this check an approve on a cancelled run would 200 while changing nothing.
    let execution = backend
        .get_execution(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    let was_paused = match &execution {
        Some(exec) => {
            if matches!(
                exec.status,
                WorkflowStatus::Completed
                    | WorkflowStatus::Failed
                    | WorkflowStatus::Cancelled
                    | WorkflowStatus::LimitExceeded
            ) {
                return Err(SubmitError::ExecutionTerminal(format!("{:?}", exec.status)));
            }
            exec.status == WorkflowStatus::Paused
        }
        None => false,
    };

    let pending = pending_approvals(&events);

    let node_id = match submission.node_id.clone().filter(|n| !n.is_empty()) {
        Some(n) => {
            if !pending.iter().any(|p| p.node_id == n) {
                if expired_by_deadline(&events, &n) {
                    return Err(SubmitError::Expired(n));
                }
                let not_pending = SubmitError::NodeNotPending(n.clone());
                return Err(refuse_decided(&events, &n, &submission, not_pending));
            }
            n
        }
        None => match pending.as_slice() {
            // Nothing pending: if the run's latest request expired, say so.
            [] => {
                return Err(match latest_requested_node(&events) {
                    Some(n) if expired_by_deadline(&events, &n) => SubmitError::Expired(n),
                    Some(n) => refuse_decided(&events, &n, &submission, SubmitError::NoPending),
                    None => SubmitError::NoPending,
                })
            }
            [only] => only.node_id.clone(),
            many => {
                return Err(SubmitError::MultiplePending(
                    many.iter().map(|p| p.node_id.clone()).collect(),
                ))
            }
        },
    };

    // IDENTITY. Outside dev mode every decision is attributable. With no
    // principal, separation of duties has nothing to compare and would fall
    // through to "allow"; refuse instead, whatever the self-approval policy.
    if !submission.dev_mode && submission.principal.is_none() {
        return Err(SubmitError::PrincipalMissing);
    }

    // SEPARATION OF DUTIES is per TOKEN, not per person. Operators mint
    // tokens (there is no REST route for it), so one human holding two tokens,
    // or a token re-minted after revocation, can approve a run they started.
    // That is a deployment control: issue one token per human.
    if matches!(submission.decision, ApprovalDecision::Approved) && !policy.allow_self_approval {
        match (run_initiator(&events), submission.principal.as_deref()) {
            (Some(initiator), Some(principal)) if initiator == principal => {
                return Err(SubmitError::SelfApproval);
            }
            // No recorded initiator: the run predates initiator recording or
            // was started in dev mode. Outside dev mode SoD cannot be proven,
            // so refuse rather than treat the run as approvable by anyone,
            // including whoever started it.
            (None, _) if !submission.dev_mode => {
                return Err(SubmitError::InitiatorUnknown);
            }
            _ => {}
        }
    }

    // BINDING. The decision answers one specific request: the latest pending
    // `ToolApprovalRequired` for this node. `node_id` was validated against
    // `pending` above, so the lookup cannot miss; it is still handled, closed.
    let Some(request) = pending.iter().find(|p| p.node_id == node_id) else {
        return Err(SubmitError::NodeNotPending(node_id));
    };
    // DEADLINE. A request past its `expires_at` is refused whether or not
    // the sweep has recorded the expiry yet: the deadline decides, not the
    // sweep's timing. An unreadable deadline counts as passed. This runs
    // BEFORE the digest rules on purpose: a late approval with no digest, or
    // a stale one, is answered 409 Expired (the request can no longer be
    // decided at all) rather than 400 DigestRequired or 409 StaleDigest.
    let now = backend
        .store_now()
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    // The store's clock from here on: this process's clock moved by how far
    // the store's was from it just now. The append's condition runs
    // synchronously and cannot ask the store, so it reads this.
    let offset = now - jamjet_state::clock::process_now();
    let store_clock = move || jamjet_state::clock::process_now() + offset;
    if request.deadline().expired(now) {
        return Err(SubmitError::Expired(node_id));
    }
    let request_digest = request.action_digest().map(str::to_owned);

    // The echo of what the reviewer was shown: see `echo_refusal`.
    if let Some(refusal) = echo_refusal(&submission, request) {
        return Err(refusal);
    }

    // CHOICE. After the deadline and the echo, so a late or stale decision
    // is told that, whatever its choice. The choice is checked against the
    // options the request showed, and those against the node's options in
    // the version the run is bound to.
    let offered = request_options(&request.context)?;
    let choice = checked_choice(&submission, &offered)?;
    if offered != declared_options(backend.as_ref(), execution.as_ref(), &node_id).await? {
        return Err(SubmitError::OptionsChanged);
    }

    // What an approval authorises: exactly the requested action. If the state
    // has moved since the request, the worker finds a different digest at
    // resume and requests afresh. Requests with no digest (agent tool
    // dispatch requests from before dispatch digests, bound by `calls_hash`
    // only, and other legacy requests) record none, and authorise nothing by
    // themselves: the guard asks again for the bound action.
    let approved_digest = match (&submission.decision, request_digest) {
        (ApprovalDecision::Approved, Some(requested)) => Some(requested),
        _ => None,
    };
    let request_sequence = Some(request.sequence);

    // ONE step from "still pending on this request" to the append: the
    // deadline sweep and any concurrent decision append under the same
    // condition, so exactly one outcome lands for a request. The sequence is
    // assigned by the backend.
    let event = Event::new(
        execution_id.clone(),
        0,
        EventKind::ApprovalReceived {
            node_id: node_id.clone(),
            user_id: submission.user_id.clone(),
            decision: submission.decision.clone(),
            comment: submission.note.clone(),
            // Refused above when non-null; never recorded.
            state_patch: None,
            principal: submission.principal.clone(),
            approved_digest,
            request_sequence,
            choice,
        },
    );
    let answered = request.sequence;
    let condition_node = node_id.clone();
    // The deadline is checked again HERE, inside the append under the
    // backend's write lock, against the store's clock at that moment: a
    // decision that passed the check above but whose append was delayed past
    // the deadline is refused, never recorded late.
    let condition = move |log: &[Event]| still_open(log, &condition_node, answered, store_clock());
    let event = match backend
        .append_event_if(event.clone(), &condition)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?
    {
        jamjet_state::ConditionalAppend::Appended(seq) => Event {
            sequence: seq,
            ..event
        },
        jamjet_state::ConditionalAppend::Refused => {
            let latest = backend
                .get_events(execution_id)
                .await
                .map_err(|e| SubmitError::Backend(e.to_string()))?;
            if expired_by_deadline(&latest, &node_id)
                || pending_but_expired(&latest, &node_id, store_clock())
            {
                return Err(SubmitError::Expired(node_id));
            }
            // The same decision, sent twice at once: the other one landed.
            if let RepeatCheck::Repeat(first) = check_repeat(&latest, &node_id, &submission) {
                return Ok(Submitted {
                    node_id,
                    event: first.clone(),
                    repeat: true,
                });
            }
            return Err(SubmitError::Superseded(node_id));
        }
        jamjet_state::ConditionalAppend::ExecutionTerminal(status) => {
            return Err(SubmitError::ExecutionTerminal(format!("{status:?}")));
        }
    };

    // Preserve the legacy paused -> running flip. Uses the pre-append status
    // read; errors propagate rather than silently skipping the flip.
    if was_paused {
        backend
            .update_execution_status(execution_id, WorkflowStatus::Running)
            .await
            .map_err(|e| SubmitError::Backend(e.to_string()))?;
    }

    Ok(Submitted {
        node_id,
        event,
        repeat: false,
    })
}

/// The node of the run's latest approval request, if any.
fn latest_requested_node(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolApprovalRequired { node_id, .. } => Some(node_id.clone()),
        _ => None,
    })
}

/// The note of a decision that may send it as `note` or under its old name
/// `comment`. Both sent and different, even only in blanks, is
/// [`SubmitError::BadNote`]: which one was meant is not guessed.
pub fn note_from(
    note: Option<String>,
    comment: Option<String>,
) -> Result<Option<String>, SubmitError> {
    match (note, comment) {
        (Some(note), Some(comment)) if note != comment => Err(SubmitError::BadNote(
            "note and comment differ; comment is the old name of note, send one of them"
                .to_string(),
        )),
        (note, comment) => Ok(note.or(comment)),
    }
}

/// The note as recorded. Absent, empty or blank is none. Any other note is
/// kept exactly as sent, if it is at most [`MAX_NOTE_BYTES`] bytes and holds
/// no [`jamjet_ir::is_forbidden_identifier_char`] other than a newline, a
/// carriage return or a tab.
fn checked_note(note: Option<String>) -> Result<Option<String>, SubmitError> {
    let Some(note) = note.filter(|n| !n.trim().is_empty()) else {
        return Ok(None);
    };
    if note.len() > MAX_NOTE_BYTES {
        return Err(SubmitError::BadNote(format!(
            "the note is {} bytes; a note is at most {MAX_NOTE_BYTES} bytes",
            note.len()
        )));
    }
    if note
        .chars()
        .any(|c| !matches!(c, '\n' | '\r' | '\t') && jamjet_ir::is_forbidden_identifier_char(c))
    {
        return Err(SubmitError::BadNote(
            "the note may not hold a control character other than newline, carriage return \
             and tab, or an invisible format character"
                .to_string(),
        ));
    }
    Ok(Some(note))
}

/// The options a request shows, from its context, in order. None for a
/// request without them, as every request before options existed. A list
/// that is not exactly what the guard writes (one this build cannot read,
/// or one with fields an option does not have) is
/// [`SubmitError::OptionsChanged`].
fn request_options(context: &serde_json::Value) -> Result<Vec<ApprovalOption>, SubmitError> {
    let Some(raw) = context.get(jamjet_worker::node_guard::OPTIONS_KEY) else {
        return Ok(Vec::new());
    };
    match serde_json::from_value::<Vec<ApprovalOption>>(raw.clone()) {
        Ok(options) if serde_json::to_value(&options).ok().as_ref() == Some(raw) => Ok(options),
        _ => Err(SubmitError::OptionsChanged),
    }
}

/// The choice this decision records, given the options its request offers.
/// An approval of a request with options names one of their ids, byte for
/// byte: nothing is trimmed or case-folded. A rejection, and any decision on
/// a request without options, takes none.
fn checked_choice(
    s: &ApprovalSubmission,
    offered: &[ApprovalOption],
) -> Result<Option<String>, SubmitError> {
    let ids = || offered.iter().map(|o| o.id.clone()).collect::<Vec<_>>();
    match (&s.decision, s.choice.as_deref()) {
        (ApprovalDecision::Rejected, Some(_)) => {
            Err(SubmitError::ChoiceNotAllowed(REJECTION_TAKES_NO_CHOICE))
        }
        (ApprovalDecision::Rejected, None) => Ok(None),
        (ApprovalDecision::Approved, Some(_)) if offered.is_empty() => {
            Err(SubmitError::ChoiceNotAllowed(NO_OPTIONS_TAKES_NO_CHOICE))
        }
        (ApprovalDecision::Approved, None) if offered.is_empty() => Ok(None),
        (ApprovalDecision::Approved, None) => Err(SubmitError::ChoiceRequired(ids())),
        (ApprovalDecision::Approved, Some(choice)) => {
            if offered.iter().any(|o| o.id == choice) {
                Ok(Some(choice.to_string()))
            } else {
                Err(SubmitError::ChoiceNotOffered(ids()))
            }
        }
    }
}

/// The options `node` declares in the workflow version the run is bound to,
/// held to the rule registration holds them to. None when the run, its
/// version or the node is not in the store: then nothing is declared, so a
/// request that shows options is refused. A node this build cannot read, or
/// options that break the rule, are [`SubmitError::OptionsChanged`]: the
/// guard raises no request for either.
async fn declared_options(
    backend: &dyn StateBackend,
    execution: Option<&WorkflowExecution>,
    node: &str,
) -> Result<Vec<ApprovalOption>, SubmitError> {
    let Some(execution) = execution else {
        return Ok(Vec::new());
    };
    let definition = backend
        .get_workflow(&execution.workflow_id, &execution.workflow_version)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    let Some(raw) = definition
        .as_ref()
        .and_then(|d| d.ir.get("nodes"))
        .and_then(|nodes| nodes.get(node))
    else {
        return Ok(Vec::new());
    };
    let node_def: jamjet_ir::NodeDef =
        serde_json::from_value(raw.clone()).map_err(|_| SubmitError::OptionsChanged)?;
    jamjet_worker::node_guard::approval_options(&node_def.kind)
        .map(<[ApprovalOption]>::to_vec)
        .map_err(|_| SubmitError::OptionsChanged)
}

/// The refusal of an echo, for a decision on `request`; `None` when the echo
/// is fine. A new decision and a repeat meet the same rule.
///
/// A `request_sequence` that is sent must be `request`'s, on either decision,
/// or it is [`SubmitError::StaleRequest`]. It is what keeps a retry from
/// deciding a request the node raised since with the same binding.
///
/// What the request binds is its action digest, or for an agent tool dispatch
/// its `calls_hash` if it predates dispatch digests. Outside dev mode an
/// approval must say which request it answers, so it can never land on a
/// request that replaced the one the reviewer saw: no echo is
/// [`SubmitError::DigestRequired`]. Rejections need no echo: refusing to act
/// is never the risk. A legacy request binds nothing, so there is nothing to
/// echo; approving it authorises nothing by itself (the worker asks again
/// with a digest). An echo that is sent must be the binding, on either
/// decision, or it is [`SubmitError::StaleDigest`]: the request has been
/// superseded since the decider saw it. A request without a binding never
/// matches one.
fn echo_refusal(s: &ApprovalSubmission, request: &PendingApproval) -> Option<SubmitError> {
    if s.request_sequence
        .is_some_and(|seq| seq != request.sequence)
    {
        return Some(SubmitError::StaleRequest);
    }
    let binding = request.binding();
    match s.action_digest.as_deref() {
        Some(echoed) if binding != Some(echoed) => Some(SubmitError::StaleDigest),
        Some(_) => None,
        None if !s.dev_mode
            && matches!(s.decision, ApprovalDecision::Approved)
            && binding.is_some() =>
        {
            Some(SubmitError::DigestRequired)
        }
        None => None,
    }
}

/// The `ApprovalReceived` that decided `node`'s latest request: `None` while
/// that request is pending, never requested, or expired by its deadline.
fn latest_decision<'a>(events: &'a [Event], node: &str) -> Option<&'a Event> {
    let sequence = match node_approval_status(events, node) {
        NodeApprovalStatus::Approved { sequence, .. }
        | NodeApprovalStatus::Rejected { sequence, .. } => sequence,
        _ => return None,
    };
    // An expiry resolves a request as a rejection too; it is not a decision.
    events
        .iter()
        .find(|e| e.sequence == sequence && matches!(e.kind, EventKind::ApprovalReceived { .. }))
}

/// Rule 1: the decision on `node`'s latest request, and that request. The
/// decision must name the request it answered; one from a log written before
/// decisions did is never repeated.
fn decision_on_latest_request<'a>(
    events: &'a [Event],
    node: &str,
) -> Option<(&'a Event, PendingApproval)> {
    let decision = latest_decision(events, node)?;
    let request = answered_request(events, node)?;
    match decision.kind {
        EventKind::ApprovalReceived {
            request_sequence: Some(answered),
            ..
        } if answered == request.sequence => Some((decision, request)),
        _ => None,
    }
}

/// Rule 2: the decision on record was made by this caller. Both carry the
/// same principal; or, in dev mode, neither carries one and the recorded user
/// is the same. One with a principal and one without never match.
fn same_decider(principal: Option<&str>, user_id: &str, s: &ApprovalSubmission) -> bool {
    match (principal, s.principal.as_deref()) {
        (Some(recorded), Some(caller)) => recorded == caller,
        (None, None) => s.dev_mode && user_id == s.user_id,
        _ => false,
    }
}

/// A decision's note as a repeat compares it: absent, empty or blank is no
/// note; any other text compares byte for byte, as sent. A submission's note
/// is already that ([`checked_note`]); a decision recorded before notes were
/// checked may still hold a blank one.
fn note_of(comment: Option<&str>) -> Option<&str> {
    comment.filter(|c| !c.trim().is_empty())
}

/// What a submission is to the decision on record for one node.
enum RepeatCheck<'a> {
    /// Not a repeat: no decision on record for the latest request, or one
    /// that differs from this submission.
    No,
    /// The same decision, but its echo is refused (rule 4).
    Refused(SubmitError),
    /// The decision on record that this submission repeats.
    Repeat(&'a Event),
}

/// Rules 1 to 4 for `node`.
fn check_repeat<'a>(events: &'a [Event], node: &str, s: &ApprovalSubmission) -> RepeatCheck<'a> {
    let Some((decision, request)) = decision_on_latest_request(events, node) else {
        return RepeatCheck::No;
    };
    let EventKind::ApprovalReceived {
        user_id,
        decision: recorded,
        comment,
        principal,
        choice,
        ..
    } = &decision.kind
    else {
        return RepeatCheck::No;
    };
    let same_decision = matches!(
        (recorded, &s.decision),
        (ApprovalDecision::Approved, ApprovalDecision::Approved)
            | (ApprovalDecision::Rejected, ApprovalDecision::Rejected)
    );
    if !same_decider(principal.as_deref(), user_id, s)
        || !same_decision
        || *choice != s.choice
        || note_of(comment.as_deref()) != note_of(s.note.as_deref())
    {
        return RepeatCheck::No;
    }
    match echo_refusal(s, &request) {
        Some(refusal) => RepeatCheck::Refused(refusal),
        None => RepeatCheck::Repeat(decision),
    }
}

/// The decision on record that this submission repeats, if any, with its
/// node. `Err` when it would repeat one but its echo is refused, and when it
/// names no node and matches the decisions of several, or one while a
/// request is pending.
fn repeated_decision(
    events: &[Event],
    s: &ApprovalSubmission,
) -> Result<Option<(String, Event)>, SubmitError> {
    if let Some(node) = s.node_id.as_deref().filter(|n| !n.is_empty()) {
        return match check_repeat(events, node, s) {
            RepeatCheck::No => Ok(None),
            RepeatCheck::Refused(refusal) => Err(refusal),
            RepeatCheck::Repeat(first) => Ok(Some((node.to_string(), first.clone()))),
        };
    }
    // Rule 5: no node named. A repeat only if exactly one node's decision
    // matches in full and no request is pending; with none matching, the
    // submission goes the usual way, which decides the one pending request
    // if there is one.
    let mut nodes: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        })
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    let mut repeats: Vec<(String, Event)> = nodes
        .into_iter()
        .filter_map(|node| match check_repeat(events, node, s) {
            RepeatCheck::Repeat(first) => Some((node.to_string(), first.clone())),
            _ => None,
        })
        .collect();
    if repeats.is_empty() {
        return Ok(None);
    }
    // A match while a request is pending could be a retry of the decision on
    // record or a new decision on the pending request: the body cannot say
    // which, so neither is assumed.
    let pending = pending_approvals(events);
    if repeats.len() == 1 && pending.is_empty() {
        return Ok(repeats.pop());
    }
    let mut candidates: Vec<String> = repeats
        .into_iter()
        .map(|(node, _)| node)
        .chain(pending.into_iter().map(|p| p.node_id))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    Err(SubmitError::MultiplePending(candidates))
}

/// The refusal for a decision on `node` when its latest request is not
/// pending and did not expire: [`SubmitError::AlreadyDecided`] if someone
/// decided it (`by_caller` by rule 2), else `otherwise`. The same decision
/// with a refused echo gets the echo's refusal: it is not a different one.
fn refuse_decided(
    events: &[Event],
    node: &str,
    s: &ApprovalSubmission,
    otherwise: SubmitError,
) -> SubmitError {
    let Some(decision) = latest_decision(events, node) else {
        return otherwise;
    };
    if let RepeatCheck::Refused(refusal) = check_repeat(events, node, s) {
        return refusal;
    }
    let by_caller = match &decision.kind {
        EventKind::ApprovalReceived {
            principal, user_id, ..
        } => same_decider(principal.as_deref(), user_id, s),
        _ => false,
    };
    SubmitError::AlreadyDecided {
        node: node.to_string(),
        by_caller,
    }
}

/// The answer to a decision, the same from the REST route and the MCP tool,
/// first time and repeat alike. `sequence` is the decision's on record.
/// `waiting_for` is read after it ([`crate::waiting::waiting_for`]); if that
/// read fails the field is left out, since the decision is recorded either way.
pub async fn decision_answer(
    backend: &dyn StateBackend,
    execution_id: &ExecutionId,
    submitted: &Submitted,
) -> serde_json::Value {
    let mut answer = serde_json::json!({
        "execution_id": execution_id.to_string(),
        "node_id": submitted.node_id,
        "accepted": true,
        "repeat": submitted.repeat,
        "sequence": submitted.event.sequence,
    });
    let waiting = match backend.get_execution(execution_id).await {
        Ok(Some(execution)) => crate::waiting::waiting_for(backend, &execution, chrono::Utc::now())
            .await
            .map_err(|e| e.to_string()),
        Ok(None) => Err("the execution was not found".to_string()),
        Err(e) => Err(e.to_string()),
    };
    match waiting {
        Ok(waiting) => answer["waiting_for"] = serde_json::json!(waiting),
        Err(e) => tracing::warn!(
            execution_id = %execution_id,
            "decision answered without waiting_for: {e}"
        ),
    }
    answer
}

/// Pending + decided approval view for `GET /executions/:id/approvals`.
pub fn approvals_view(events: &[Event]) -> serde_json::Value {
    let pending: Vec<serde_json::Value> = pending_approvals(events)
        .iter()
        .map(|p| {
            let mut item = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);
            show_options(&mut item, Some(&p.context));
            item
        })
        .collect();
    let pending_nodes: Vec<String> = pending_approvals(events)
        .into_iter()
        .map(|p| p.node_id)
        .collect();

    // Collect every node that ever had a `ToolApprovalRequired`.
    let mut nodes: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect();
    nodes.sort();
    nodes.dedup();

    let decided: Vec<serde_json::Value> = nodes
        .iter()
        .filter(|n| !pending_nodes.contains(*n))
        .map(|n| match node_approval_status(events, n) {
            NodeApprovalStatus::Approved {
                user_id,
                sequence,
                approved_digest,
            } => {
                let (choice, note) = decision_at(events, sequence);
                serde_json::json!({
                    "node_id": n,
                    "status": "approved",
                    "user_id": user_id,
                    "sequence": sequence,
                    "approved_digest": approved_digest,
                    "choice": choice,
                    "note": note,
                })
            }
            NodeApprovalStatus::Rejected {
                user_id,
                comment,
                sequence,
            } => serde_json::json!({
                "node_id": n,
                "status": "rejected",
                "user_id": user_id,
                "comment": comment,
                "note": comment,
                "choice": serde_json::Value::Null,
                "sequence": sequence,
            }),
            // Unreachable in practice: a node filtered out of `pending` has a
            // decision by construction. Kept total so the view never panics.
            _ => serde_json::json!({ "node_id": n, "status": "unknown" }),
        })
        .collect();

    serde_json::json!({ "pending": pending, "decided": decided })
}

/// Copy the options a request offers from its context to an `options` key of
/// the listing item, so a reviewer finds them without reading the context.
/// Nothing is added for a request without options.
pub fn show_options(item: &mut serde_json::Value, context: Option<&serde_json::Value>) {
    if let Some(options) = context.and_then(|c| c.get("options")) {
        item["options"] = options.clone();
    }
}

/// The `choice` and `note` of the decision recorded at `sequence`; null for
/// each the decision does not carry.
fn decision_at(events: &[Event], sequence: i64) -> (serde_json::Value, serde_json::Value) {
    events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ApprovalReceived {
                choice, comment, ..
            } if e.sequence == sequence => {
                Some((serde_json::json!(choice), serde_json::json!(comment)))
            }
            _ => None,
        })
        .unwrap_or((serde_json::Value::Null, serde_json::Value::Null))
}

/// One row of `GET /approvals`: a request waiting for a decision, and what a
/// reviewer echoes to decide it.
///
/// `action_digest` is the request's binding, the echo the approve route
/// accepts: its `action_digest`, or for an older agent tool dispatch request
/// its `calls_hash`, or null for a legacy request that binds nothing. The two
/// deadline keys are always there, null when the request's policy set none.
/// `context` is the request's context, whole.
pub fn pending_row_view(row: &PendingApprovalRow) -> serde_json::Value {
    let context = row.context.clone().unwrap_or(serde_json::Value::Null);
    let request = PendingApproval::from_request(
        &row.node_id,
        row.tool_name.as_deref().unwrap_or_default(),
        row.approver.as_deref().unwrap_or_default(),
        &context,
        row.request_sequence,
    );
    let deadline = |key: &str| context.get(key).cloned().unwrap_or(serde_json::Value::Null);
    let mut view = serde_json::json!({
        "execution_id": row.execution_id.to_string(),
        "workflow_id": row.workflow_id,
        "workflow_version": row.workflow_version,
        "node_id": row.node_id,
        "tool_name": row.tool_name,
        "approver": row.approver,
        "request_sequence": row.request_sequence,
        "action_digest": request.binding(),
        "expires_at": deadline(EXPIRES_AT_KEY),
        "escalate_at": deadline(ESCALATE_AT_KEY),
        "context": row.context,
    });
    show_options(&mut view, row.context.as_ref());
    view
}

/// The `next_after` of `GET /approvals`: lowercase hex of the cursor's JSON,
/// so it goes in a query string as it is.
pub fn encode_approval_cursor(cursor: &ApprovalCursor) -> serde_json::Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let json = serde_json::to_vec(cursor)?;
    let mut out = String::with_capacity(json.len() * 2);
    for b in json {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    Ok(out)
}

/// The cursor an `after` names. `None` for anything that is not an even
/// number of hex digits spelling the JSON of an [`ApprovalCursor`].
pub fn decode_approval_cursor(text: &str) -> Option<ApprovalCursor> {
    let (pairs, odd) = text.as_bytes().as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    let nibble = |d: u8| char::from(d).to_digit(16);
    let mut json = Vec::with_capacity(pairs.len());
    for &[high, low] in pairs {
        let byte = (nibble(high)? << 4) | nibble(low)?;
        json.push(u8::try_from(byte).ok()?);
    }
    serde_json::from_slice(&json).ok()
}
