//! Graceful shutdown for the in-process workers and the scheduler.
//!
//! [`shutdown`] returns a [`ShutdownTrigger`] for the process's signal handler
//! and a [`Shutdown`] to hand to whatever loops. On
//! [`ShutdownTrigger::stop_and_drain`] each loop finishes the item or tick in
//! hand, starts nothing new and exits, and the trigger waits (up to a grace
//! period) for every worker to do so. Until 2026-11 the server waited only
//! for Ctrl-C, so a SIGTERM (what Docker, Kubernetes and systemd send) killed
//! it mid-node, and the workers were detached tasks nothing waited for.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};

/// How long shutdown waits for the in-process workers by default
/// (`JAMJET_SHUTDOWN_GRACE_SECS`).
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// How long shutdown then waits for the scheduler to stop between ticks.
pub const SCHEDULER_STOP: Duration = Duration::from_secs(2);

/// How long shutdown then waits for open HTTP connections.
pub const HTTP_DRAIN: Duration = Duration::from_secs(3);

/// The longest a default shutdown takes, which must fit Kubernetes' default
/// 30 s between SIGTERM and SIGKILL with room to spare.
pub const MAX_DEFAULT_SHUTDOWN: Duration = Duration::from_secs(25);

#[derive(Default)]
struct Running {
    count: AtomicUsize,
    changed: Notify,
}

/// The receiving side: loops watch [`Shutdown::receiver`], and the pool
/// counts its workers through it.
#[derive(Clone)]
pub struct Shutdown {
    stop: watch::Receiver<bool>,
    running: Arc<Running>,
}

/// The sending side, for the signal handler.
pub struct ShutdownTrigger {
    stop: watch::Sender<bool>,
    running: Arc<Running>,
}

/// A connected trigger and receiver.
pub fn shutdown() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    let running = Arc::new(Running::default());
    (
        ShutdownTrigger {
            stop: tx,
            running: Arc::clone(&running),
        },
        Shutdown { stop: rx, running },
    )
}

impl Shutdown {
    /// Turns true when shutdown starts.
    pub fn receiver(&self) -> watch::Receiver<bool> {
        self.stop.clone()
    }

    /// Count one running worker until the guard drops.
    pub(crate) fn enter(&self) -> RunningGuard {
        self.running.count.fetch_add(1, Ordering::SeqCst);
        RunningGuard(Arc::clone(&self.running))
    }
}

/// Held by a running worker task; dropping it (the task ending, by return or
/// by panic) counts the worker out.
pub(crate) struct RunningGuard(Arc<Running>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}

impl ShutdownTrigger {
    /// Signal every loop to stop, then wait up to `grace` for the workers to
    /// finish the items they hold. Returns how many were still running when
    /// the grace period ran out (0: drained). An item abandoned that way
    /// keeps its lease until it expires, and is then reclaimed and run again.
    pub async fn stop_and_drain(self, grace: Duration) -> usize {
        let _ = self.stop.send(true);
        let running = Arc::clone(&self.running);
        let drained = async move {
            loop {
                let notified = running.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if running.count.load(Ordering::SeqCst) == 0 {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(grace, drained).await;
        self.running.count.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod default_timings {
    use super::*;

    /// Grace, scheduler and HTTP drain together, with the defaults: within
    /// 25 s. The grace alone used to be 30 s, past Kubernetes' default.
    #[test]
    fn a_default_shutdown_fits_a_thirty_second_kill() {
        assert!(DEFAULT_SHUTDOWN_GRACE + SCHEDULER_STOP + HTTP_DRAIN <= MAX_DEFAULT_SHUTDOWN);
    }
}
