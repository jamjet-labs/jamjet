use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("workflow not found: {0}")]
    WorkflowNotFound(String),

    #[error("execution not found: {0}")]
    ExecutionNotFound(String),

    #[error("node not found: {node_id} in workflow {workflow_id}")]
    NodeNotFound {
        workflow_id: String,
        node_id: String,
    },

    #[error("invalid state transition: {current:?} → {requested:?}")]
    InvalidTransition {
        current: crate::WorkflowStatus,
        requested: crate::WorkflowStatus,
    },

    #[error("schema validation failed: {0}")]
    SchemaValidation(String),

    #[error("policy violation: {0}")]
    PolicyViolation(String),

    #[error("budget exceeded: {kind} limit reached (limit={limit}, current={current})")]
    BudgetExceeded {
        kind: String,
        limit: u64,
        current: u64,
    },

    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
