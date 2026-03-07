//! JamJet Scheduler
//!
//! The scheduler drives workflow execution:
//! 1. Detect which nodes are runnable (all predecessors completed)
//! 2. Dispatch runnable nodes to the appropriate worker queue
//! 3. Monitor worker leases and re-queue timed-out items
//! 4. Handle retry scheduling on node failure
//! 5. Wake suspended executions on timer/external-event

pub mod runner;

pub use runner::Scheduler;
