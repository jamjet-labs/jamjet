//! JamJet Worker
//!
//! Workers are separate processes (or tasks) that:
//! - Pull work items from the queue
//! - Acquire leases (preventing duplicate execution)
//! - Execute node logic (model calls, tool calls, Python functions, MCP, A2A)
//! - Emit heartbeats to renew leases
//! - Report results back via the state backend

pub mod executor;
pub mod executors;
pub mod heartbeat;
pub mod pool;
pub mod worker;

pub use executor::{ExecutionResult, NodeExecutor};
pub use executors::{A2aTaskExecutor, AgentDiscoveryExecutor, McpToolExecutor, ModelNodeExecutor};
pub use pool::{default_pool, WorkerGroupConfig, WorkerPool};
pub use worker::Worker;
