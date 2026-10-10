//! JamJet Scheduler
//!
//! The scheduler drives workflow execution:
//! 1. Detect which nodes are runnable (all predecessors completed)
//! 2. Dispatch runnable nodes to the appropriate worker queue
//! 3. Monitor worker leases and re-queue timed-out items
//! 4. Handle retry scheduling on node failure
//! 5. Wake suspended executions on timer/external-event

pub mod payload;
pub mod projector;
pub mod runner;
pub mod strategy_bridge;

pub use payload::{build_work_item_payload, max_attempts_for};
pub use projector::Projector;
pub use runner::{DeadlineSweep, Scheduler, SchedulerConfig};
