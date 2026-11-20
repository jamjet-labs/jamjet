//! Fault points: places inside the store's transactions where a test can
//! make the store fail, hang, or abort the process.
//!
//! A point follows each statement of each multi-statement transaction,
//! numbered from 1 in the order the statements appear in the code, under one
//! label per transaction body: `sqlite.commit_scheduler_pass#2` is the point
//! after the second statement of the SQLite scheduler pass. A call to a
//! helper that runs statements of its own counts as one statement. A point
//! inside a loop is reached on every pass, so an armed error stops the
//! transaction on the first.
//!
//! Each commit has a point on the line before it, outside any loop or branch
//! the commit is not in. Where the last statement's point is not there
//! already, it sits after the check of what that statement did, or the
//! commit has a point of its own, numbered after the rest. An error armed
//! there stops the call once every statement before the commit has run, so
//! it finds a write that a commit split off anywhere earlier in the body, in
//! a loop's later passes too. [`POINTS`] lists every point the code
//! declares, and `testkit/tests/fault_points_listed.rs` checks it against
//! the `fault_point!` calls in the sources, and checks that each commit has
//! its point.
//!
//! This module is compiled in every build, so callers compile either way.
//! Only the `test-hooks` feature of this crate makes a point do anything:
//! without it [`hit`] returns `Ok(())` and arming does nothing.
//!
//! A test that has its process to itself arms points for the whole process,
//! with [`arm`], or with [`arm_from_env`] in a server the test starts. A
//! test that shares its binary with other tests arms them in a [`Scope`],
//! and the faults reach only the calls it runs inside [`Scope::run`].

use crate::backend::BackendResult;
#[cfg(feature = "test-hooks")]
use crate::backend::StateBackendError;
use std::time::Duration;

/// What an armed point does when a call reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The call fails with `Database("fault injected at <label>#<step>")`.
    /// The transaction is dropped uncommitted, as for any error there.
    Error,
    /// The call waits this long at the point, holding whatever its
    /// transaction holds, then goes on.
    Hang(Duration),
    /// The process prints the point on stderr and aborts: no destructors
    /// run and the connection drops, so to the database it is a kill -9.
    Abort,
}

/// Every point the code declares, as `(label, step)`.
pub const POINTS: &[(&str, u32)] = &[
    // sqlite.rs: the unscoped bodies.
    ("sqlite.commit_turn", 1),
    ("sqlite.commit_turn", 2),
    ("sqlite.commit_turn", 3),
    ("sqlite.commit_turn", 4),
    ("sqlite.commit_turn", 5),
    ("sqlite.commit_turn", 6),
    ("sqlite.commit_turn", 7),
    ("sqlite.commit_turn", 8),
    ("sqlite.commit_turn", 9),
    ("sqlite.commit_turn", 10),
    ("sqlite.commit_turn", 11),
    ("sqlite.append_event", 1),
    ("sqlite.append_event", 2),
    ("sqlite.claim_work_item", 1),
    ("sqlite.claim_work_item", 2),
    ("sqlite.claim_work_item", 3),
    ("sqlite.fail_work_item_fenced", 1),
    ("sqlite.fail_work_item_fenced", 2),
    ("sqlite.fail_work_item_fenced", 3),
    ("sqlite.fail_work_item_fenced", 4),
    ("sqlite.finalize_rollover_fenced", 1),
    ("sqlite.finalize_rollover_fenced", 2),
    ("sqlite.apply_approval_projection", 1),
    ("sqlite.apply_approval_projection", 2),
    ("sqlite.apply_approval_projection_batch", 1),
    ("sqlite.apply_approval_projection_batch", 2),
    // sqlite.rs: the bodies both SQLite backends share.
    ("sqlite.create_run", 1),
    ("sqlite.create_run", 2),
    ("sqlite.create_run", 3),
    ("sqlite.create_run", 4),
    ("sqlite.create_run", 5),
    ("sqlite.finish_execution", 1),
    ("sqlite.finish_execution", 2),
    ("sqlite.finish_execution", 3),
    ("sqlite.finish_execution", 4),
    ("sqlite.append_event_if", 1),
    ("sqlite.append_event_if", 2),
    ("sqlite.append_event_if", 3),
    ("sqlite.expire_stale_leases", 1),
    ("sqlite.expire_stale_leases", 2),
    ("sqlite.expire_stale_leases", 3),
    ("sqlite.expire_stale_leases", 4),
    ("sqlite.expire_stale_leases", 5),
    ("sqlite.reattach_lease", 1),
    ("sqlite.reattach_lease", 2),
    ("sqlite.reattach_lease", 3),
    ("sqlite.commit_scheduler_pass", 1),
    ("sqlite.commit_scheduler_pass", 2),
    ("sqlite.commit_scheduler_pass", 3),
    ("sqlite.commit_scheduler_pass", 4),
    ("sqlite.commit_scheduler_pass", 5),
    ("sqlite.commit_scheduler_pass", 6),
    ("sqlite.commit_scheduler_pass", 7),
    ("sqlite.commit_scheduler_pass", 8),
    ("sqlite.reclaim_expired_leases_recorded", 1),
    ("sqlite.reclaim_expired_leases_recorded", 2),
    ("sqlite.reclaim_expired_leases_recorded", 3),
    ("sqlite.reclaim_expired_leases_recorded", 4),
    ("sqlite.reclaim_expired_leases_recorded", 5),
    ("sqlite.reclaim_expired_leases_recorded", 6),
    ("sqlite.reclaim_expired_leases_recorded", 7),
    ("sqlite.fail_work_item_fenced_recorded", 1),
    ("sqlite.fail_work_item_fenced_recorded", 2),
    ("sqlite.fail_work_item_fenced_recorded", 3),
    ("sqlite.fail_work_item_fenced_recorded", 4),
    ("sqlite.fail_work_item_fenced_recorded", 5),
    ("sqlite.fail_work_item_fenced_recorded", 6),
    ("sqlite.fail_work_item_fenced_recorded", 7),
    ("sqlite.patch_append_array", 1),
    ("sqlite.patch_append_array", 2),
    // tenant_scoped.rs: the scoped copies of the unscoped bodies.
    ("sqlite_scoped.commit_turn", 1),
    ("sqlite_scoped.commit_turn", 2),
    ("sqlite_scoped.commit_turn", 3),
    ("sqlite_scoped.commit_turn", 4),
    ("sqlite_scoped.commit_turn", 5),
    ("sqlite_scoped.commit_turn", 6),
    ("sqlite_scoped.commit_turn", 7),
    ("sqlite_scoped.commit_turn", 8),
    ("sqlite_scoped.commit_turn", 9),
    ("sqlite_scoped.commit_turn", 10),
    ("sqlite_scoped.commit_turn", 11),
    ("sqlite_scoped.append_event", 1),
    ("sqlite_scoped.append_event", 2),
    ("sqlite_scoped.claim_work_item", 1),
    ("sqlite_scoped.claim_work_item", 2),
    ("sqlite_scoped.claim_work_item", 3),
    ("sqlite_scoped.fail_work_item_fenced", 1),
    ("sqlite_scoped.fail_work_item_fenced", 2),
    ("sqlite_scoped.fail_work_item_fenced", 3),
    ("sqlite_scoped.fail_work_item_fenced", 4),
    ("sqlite_scoped.finalize_rollover_fenced", 1),
    ("sqlite_scoped.finalize_rollover_fenced", 2),
    ("sqlite_scoped.apply_approval_projection", 1),
    ("sqlite_scoped.apply_approval_projection", 2),
    ("sqlite_scoped.apply_approval_projection_batch", 1),
    ("sqlite_scoped.apply_approval_projection_batch", 2),
];

/// A fault point: the boundary after statement `$step` of the transaction
/// named `$label`. It goes in an async function that returns a
/// [`BackendResult`](crate::BackendResult), and passes an injected error up
/// with `?`.
#[macro_export]
macro_rules! fault_point {
    // No #[cfg] in the expansion: it would test the features of the crate
    // that calls the macro, not those of jamjet-state.
    ($label:literal, $step:literal) => {
        $crate::faults::hit($label, $step).await?
    };
}

/// Whether this build has the `test-hooks` feature, so armed points fire.
pub const fn enabled() -> bool {
    cfg!(feature = "test-hooks")
}

/// What [`fault_point!`] calls. `Ok(())` unless a fault is armed at the
/// point, in the calling task's [`Scope`] if it runs in one, otherwise for
/// the process.
#[cfg(feature = "test-hooks")]
pub async fn hit(label: &'static str, step: u32) -> BackendResult<()> {
    match registry::armed_at(label, step) {
        None => Ok(()),
        Some(Fault::Error) => Err(StateBackendError::Database(format!(
            "fault injected at {label}#{step}"
        ))),
        Some(Fault::Hang(how_long)) => {
            tokio::time::sleep(how_long).await;
            Ok(())
        }
        Some(Fault::Abort) => {
            eprintln!("fault point {label}#{step}: abort");
            std::process::abort();
        }
    }
}

/// What [`fault_point!`] calls: without the feature, `Ok(())` at once. The
/// future holds nothing, so a build without the feature keeps no trace of
/// the points, their labels included.
#[cfg(not(feature = "test-hooks"))]
#[inline(always)]
pub fn hit(label: &'static str, step: u32) -> std::future::Ready<BackendResult<()>> {
    let _ = (label, step);
    std::future::ready(Ok(()))
}

/// Arm `fault` at a point for the whole process, outside every [`Scope`].
/// Panics if the point is not in [`POINTS`].
pub fn arm(label: &'static str, step: u32, fault: Fault) {
    #[cfg(feature = "test-hooks")]
    {
        declared(label, step);
        registry::lock(&registry::GLOBAL).arm(label, step, fault);
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = (label, step, fault);
}

/// Disarm every point armed for the whole process.
pub fn disarm_all() {
    #[cfg(feature = "test-hooks")]
    registry::lock(&registry::GLOBAL).disarm_all();
}

/// Arm [`Fault::Abort`] at the point `JAMJET_TEST_CRASH_AT` names, as
/// `label#step`, or as a bare `label` for its step 1. Does nothing when the
/// variable is unset, and nothing at all without the feature: the variable's
/// name is not in such a build. Panics on a name that is not in [`POINTS`],
/// so a mistyped point cannot pass for one that was never reached.
pub fn arm_from_env() {
    #[cfg(feature = "test-hooks")]
    if let Ok(spec) = std::env::var("JAMJET_TEST_CRASH_AT") {
        let (label, step) =
            point_named(&spec).unwrap_or_else(|e| panic!("JAMJET_TEST_CRASH_AT: {e}"));
        arm(label, step, Fault::Abort);
    }
}

/// The declared point `spec` names: `label#step`, or `label` for step 1.
#[cfg_attr(not(feature = "test-hooks"), allow(dead_code))]
fn point_named(spec: &str) -> Result<(&'static str, u32), String> {
    let (label, step) = match spec.split_once('#') {
        Some((label, step)) => match step.parse::<u32>() {
            Ok(step) => (label, step),
            Err(_) => return Err(format!("{spec:?}: the step is not a number")),
        },
        None => (spec, 1),
    };
    POINTS
        .iter()
        .find(|(l, s)| *l == label && *s == step)
        .copied()
        .ok_or_else(|| format!("{spec:?} names no fault point"))
}

#[cfg(feature = "test-hooks")]
fn declared(label: &str, step: u32) {
    assert!(
        POINTS.contains(&(label, step)),
        "no fault point {label}#{step} is declared in faults::POINTS"
    );
}

/// Faults armed for the calls one test runs inside [`Scope::run`], and
/// nowhere else: other tests in the same binary, and calls the test makes
/// outside `run`, do not see them. Inside `run`, the faults armed for the
/// process do not apply. Clones share their faults.
#[derive(Clone, Default)]
pub struct Scope {
    #[cfg(feature = "test-hooks")]
    registry: std::sync::Arc<std::sync::Mutex<registry::Registry>>,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm `fault` at a point for calls inside [`Scope::run`]. Panics if the
    /// point is not in [`POINTS`].
    pub fn arm(&self, label: &'static str, step: u32, fault: Fault) {
        #[cfg(feature = "test-hooks")]
        {
            declared(label, step);
            registry::lock(&self.registry).arm(label, step, fault);
        }
        #[cfg(not(feature = "test-hooks"))]
        let _ = (label, step, fault);
    }

    /// Disarm every point armed in this scope.
    pub fn disarm_all(&self) {
        #[cfg(feature = "test-hooks")]
        registry::lock(&self.registry).disarm_all();
    }

    /// How many times a call in this scope reached the point while a fault
    /// was armed there.
    pub fn fired(&self, label: &'static str, step: u32) -> u64 {
        #[cfg(feature = "test-hooks")]
        {
            registry::lock(&self.registry).fired(label, step)
        }
        #[cfg(not(feature = "test-hooks"))]
        {
            let _ = (label, step);
            0
        }
    }

    /// Run `work` with this scope's faults.
    pub async fn run<F: std::future::Future>(&self, work: F) -> F::Output {
        #[cfg(feature = "test-hooks")]
        {
            registry::SCOPED.scope(self.registry.clone(), work).await
        }
        #[cfg(not(feature = "test-hooks"))]
        {
            work.await
        }
    }
}

#[cfg(feature = "test-hooks")]
mod registry {
    use super::Fault;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    /// The armed points, and how often each fired.
    #[derive(Default)]
    pub(super) struct Registry {
        armed: BTreeMap<(&'static str, u32), Fault>,
        fired: BTreeMap<(&'static str, u32), u64>,
    }

    impl Registry {
        const fn new() -> Self {
            Self {
                armed: BTreeMap::new(),
                fired: BTreeMap::new(),
            }
        }

        pub(super) fn arm(&mut self, label: &'static str, step: u32, fault: Fault) {
            self.armed.insert((label, step), fault);
        }

        pub(super) fn disarm_all(&mut self) {
            self.armed.clear();
        }

        pub(super) fn fired(&self, label: &'static str, step: u32) -> u64 {
            self.fired.get(&(label, step)).copied().unwrap_or(0)
        }

        /// The fault armed at the point, counted as fired.
        fn reached(&mut self, label: &'static str, step: u32) -> Option<Fault> {
            let fault = self.armed.get(&(label, step)).copied()?;
            *self.fired.entry((label, step)).or_default() += 1;
            Some(fault)
        }
    }

    /// The faults armed for the process.
    pub(super) static GLOBAL: Mutex<Registry> = Mutex::new(Registry::new());

    tokio::task_local! {
        /// The faults of the [`Scope`](super::Scope) the task runs in.
        pub(super) static SCOPED: Arc<Mutex<Registry>>;
    }

    /// A panic while the lock was held leaves the map whole, so a poisoned
    /// lock is used as it is.
    pub(super) fn lock(registry: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
        registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The fault armed at the point for the calling task: its scope's if it
    /// runs in one, otherwise the process's.
    pub(super) fn armed_at(label: &'static str, step: u32) -> Option<Fault> {
        SCOPED
            .try_with(|scoped| lock(scoped).reached(label, step))
            .unwrap_or_else(|_| lock(&GLOBAL).reached(label, step))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_is_named_with_or_without_its_step() {
        let &(label, step) = POINTS.last().expect("the code declares fault points");
        assert_eq!(point_named(&format!("{label}#{step}")), Ok((label, step)));
        let first = POINTS
            .iter()
            .find(|(_, s)| *s == 1)
            .expect("a label has a step 1")
            .0;
        assert_eq!(point_named(first), Ok((first, 1)));
        assert!(point_named(&format!("{label}#{}", step + 1000)).is_err());
        assert!(point_named(&format!("{label}#x")).is_err());
        assert!(point_named("no.such_point").is_err());
    }
}
