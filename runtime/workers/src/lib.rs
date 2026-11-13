//! JamJet Worker
//!
//! Workers are separate processes (or tasks) that:
//! - Pull work items from the queue
//! - Acquire leases (preventing duplicate execution)
//! - Execute node logic (model calls, tool calls, Python functions, MCP, A2A)
//! - Emit heartbeats to renew leases
//! - Report results back via the state backend

pub mod agent_limits;
pub mod dispatch_guard;
pub mod executable;
pub mod executor;
pub mod executors;
pub mod heartbeat;
pub mod node_guard;
pub mod pool;
pub mod shutdown;
#[cfg(test)]
#[cfg(test)]
pub(crate) mod test_support;
pub mod worker;

pub use dispatch_guard::{guard_dispatch, DispatchGuardOutcome};
pub use executor::{ExecutionResult, ExecutorError, NodeExecutor};
pub use executors::{
    A2aTaskExecutor, AgentDiscoveryExecutor, ConditionNodeExecutor, EvalExecutor,
    HumanApprovalExecutor, LimitExceededExecutor, McpToolExecutor, ModelNodeExecutor,
};
pub use pool::{default_pool, server_pool, WorkerGroupConfig, WorkerPool};
pub use shutdown::{shutdown, Shutdown, ShutdownTrigger};
pub use worker::Worker;
