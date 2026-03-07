use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a workflow definition.
pub type WorkflowId = String;

/// Unique identifier for a workflow execution instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(pub Uuid);

impl ExecutionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec_{}", self.0.simple())
    }
}

/// The lifecycle status of a workflow execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    /// Created, not yet started.
    Pending,
    /// One or more nodes are active or queued.
    Running,
    /// Paused waiting for: human approval, external event, or timer.
    Paused,
    /// All terminal nodes reached successfully.
    Completed,
    /// A node failed beyond its retry policy.
    Failed,
    /// Explicitly cancelled.
    Cancelled,
}

impl WorkflowStatus {
    /// Returns true if this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Returns true if this execution can still accept work.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Running | Self::Paused)
    }

    /// Validate a state transition. Returns Ok(()) if the transition is valid.
    pub fn validate_transition(&self, next: &WorkflowStatus) -> crate::error::Result<()> {
        let valid = match (self, next) {
            (Self::Pending, Self::Running) => true,
            (Self::Running, Self::Paused) => true,
            (Self::Running, Self::Completed) => true,
            (Self::Running, Self::Failed) => true,
            (Self::Running, Self::Cancelled) => true,
            (Self::Paused, Self::Running) => true,
            (Self::Paused, Self::Cancelled) => true,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(crate::Error::InvalidTransition {
                current: self.clone(),
                requested: next.clone(),
            })
        }
    }
}

/// Metadata for a workflow definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowMetadata {
    pub id: WorkflowId,
    pub version: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub state_schema: String,
    pub created_at: DateTime<Utc>,
}

/// A running execution of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    pub execution_id: ExecutionId,
    pub workflow_id: WorkflowId,
    pub workflow_version: String,
    pub status: WorkflowStatus,
    pub initial_input: serde_json::Value,
    pub current_state: serde_json::Value,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        let s = WorkflowStatus::Pending;
        assert!(s.validate_transition(&WorkflowStatus::Running).is_ok());
    }

    #[test]
    fn invalid_transitions() {
        let s = WorkflowStatus::Completed;
        assert!(s.validate_transition(&WorkflowStatus::Running).is_err());
    }

    #[test]
    fn terminal_states() {
        assert!(WorkflowStatus::Completed.is_terminal());
        assert!(WorkflowStatus::Failed.is_terminal());
        assert!(WorkflowStatus::Cancelled.is_terminal());
        assert!(!WorkflowStatus::Running.is_terminal());
        assert!(!WorkflowStatus::Paused.is_terminal());
    }

    #[test]
    fn execution_id_display() {
        let id = ExecutionId::new();
        assert!(id.to_string().starts_with("exec_"));
    }
}
