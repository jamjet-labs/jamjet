//! JamJet Policy Engine (Phase 4)
//!
//! Evaluates policy rules at node execution time:
//! - Block disallowed tools
//! - Require approval for sensitive operations
//! - Enforce model allowlists
//! - Validate structured output classes

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    Block { reason: String },
    RequireApproval { approver: String },
}
