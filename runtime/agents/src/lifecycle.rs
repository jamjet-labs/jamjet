use serde::{Deserialize, Serialize};

/// Lifecycle state of a registered agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Registered but not yet accepting tasks.
    Registered,
    /// Accepting and executing tasks.
    Active,
    /// Not accepting new tasks; existing tasks continue.
    Paused,
    /// Draining in-flight tasks; will become archived once drained.
    Deactivated,
    /// No longer active. Kept for historical reference.
    Archived,
}

impl AgentStatus {
    pub fn can_accept_tasks(&self) -> bool {
        matches!(self, Self::Active)
    }

    pub fn validate_transition(&self, next: &AgentStatus) -> Result<(), String> {
        let valid = matches!(
            (self, next),
            (Self::Registered, Self::Active)
                | (Self::Active, Self::Paused)
                | (Self::Active, Self::Deactivated)
                | (Self::Paused, Self::Active)
                | (Self::Paused, Self::Deactivated)
                | (Self::Deactivated, Self::Archived)
        );
        if valid {
            Ok(())
        } else {
            Err(format!(
                "invalid agent status transition: {self:?} → {next:?}"
            ))
        }
    }
}
