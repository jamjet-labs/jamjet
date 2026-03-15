//! Typed failure taxonomy for delegated agent operations (B.2).
//!
//! Defines canonical failure types that can occur when one agent delegates
//! work to another. Each failure carries severity, retryable flag,
//! optional partial output, and a recommended fallback action.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical delegation failure variants.
///
/// Serialises with `"type"` as the tag field, e.g.
/// `{"type": "delegate_unreachable", "url": "...", "message": "..."}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DelegationFailure {
    /// The delegated agent could not be reached.
    DelegateUnreachable {
        url: String,
        message: String,
    },
    /// The requested capability is not among those the delegate advertises.
    CapabilityMismatch {
        requested: String,
        available: Vec<String>,
    },
    /// A governance policy denied the delegation.
    PolicyDenied {
        policy_id: String,
        reason: String,
    },
    /// The delegation requires human approval before it can proceed.
    ApprovalRequired {
        prompt: String,
    },
    /// The delegation would exceed an allocated budget.
    BudgetExceeded {
        limit: f64,
        actual: f64,
        unit: String,
    },
    /// A trust-boundary crossing was denied.
    TrustCrossingDenied {
        from_domain: String,
        to_domain: String,
    },
    /// A verification check (signature, hash, attestation) failed.
    VerificationFailed {
        check: String,
        message: String,
    },
    /// A tool dependency required by the delegate failed.
    ToolDependencyFailed {
        tool: String,
        error: String,
    },
    /// The delegation partially completed before failing.
    PartialCompletion {
        completed_steps: usize,
        total_steps: usize,
        output: Option<Value>,
    },
    /// The runtime fell back to an alternate delegate.
    FallbackInvoked {
        original_delegate: String,
        fallback_delegate: String,
        reason: String,
    },
    /// The delegation timed out.
    Timeout {
        deadline_secs: u64,
        elapsed_secs: u64,
    },
}

/// Severity classification for delegation failures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureSeverity {
    /// Non-fatal; the workflow can continue with degraded output.
    Warning,
    /// An error that prevents this delegation from succeeding but does not
    /// necessarily terminate the workflow.
    Error,
    /// A fatal error that should abort the entire workflow.
    Fatal,
}

/// Enriched failure envelope carrying metadata alongside the failure variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationFailureInfo {
    /// The specific failure that occurred.
    pub failure: DelegationFailure,
    /// How severe the failure is.
    pub severity: FailureSeverity,
    /// Whether the caller should retry.
    pub retryable: bool,
    /// Partial output produced before the failure, if any.
    pub partial_output: Option<Value>,
    /// A recommended fallback agent or strategy identifier.
    pub recommended_fallback: Option<String>,
    /// An opaque reference for the audit log.
    pub audit_ref: Option<String>,
    /// ISO-8601 timestamp of when the failure occurred.
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_delegate_unreachable() {
        let f = DelegationFailure::DelegateUnreachable {
            url: "https://agent.example.com".into(),
            message: "connection refused".into(),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"delegate_unreachable""#));
        let back: DelegationFailure = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn round_trip_failure_info() {
        let info = DelegationFailureInfo {
            failure: DelegationFailure::Timeout {
                deadline_secs: 30,
                elapsed_secs: 35,
            },
            severity: FailureSeverity::Error,
            retryable: true,
            partial_output: Some(serde_json::json!({"partial": true})),
            recommended_fallback: Some("backup-agent".into()),
            audit_ref: Some("audit-12345".into()),
            timestamp: "2026-03-15T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: DelegationFailureInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.severity, FailureSeverity::Error);
        assert!(back.retryable);
    }

    #[test]
    fn severity_round_trip() {
        for sev in [
            FailureSeverity::Warning,
            FailureSeverity::Error,
            FailureSeverity::Fatal,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let back: FailureSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(sev, back);
        }
    }

    #[test]
    fn all_variants_serialize() {
        let variants: Vec<DelegationFailure> = vec![
            DelegationFailure::DelegateUnreachable {
                url: "u".into(),
                message: "m".into(),
            },
            DelegationFailure::CapabilityMismatch {
                requested: "r".into(),
                available: vec!["a".into()],
            },
            DelegationFailure::PolicyDenied {
                policy_id: "p".into(),
                reason: "r".into(),
            },
            DelegationFailure::ApprovalRequired {
                prompt: "ok?".into(),
            },
            DelegationFailure::BudgetExceeded {
                limit: 10.0,
                actual: 15.0,
                unit: "usd".into(),
            },
            DelegationFailure::TrustCrossingDenied {
                from_domain: "a.com".into(),
                to_domain: "b.com".into(),
            },
            DelegationFailure::VerificationFailed {
                check: "sig".into(),
                message: "bad".into(),
            },
            DelegationFailure::ToolDependencyFailed {
                tool: "t".into(),
                error: "e".into(),
            },
            DelegationFailure::PartialCompletion {
                completed_steps: 3,
                total_steps: 5,
                output: None,
            },
            DelegationFailure::FallbackInvoked {
                original_delegate: "a".into(),
                fallback_delegate: "b".into(),
                reason: "r".into(),
            },
            DelegationFailure::Timeout {
                deadline_secs: 10,
                elapsed_secs: 12,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            assert!(json.contains("\"type\""));
            let back: DelegationFailure = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }
}
