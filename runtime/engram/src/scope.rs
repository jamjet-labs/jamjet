//! `Scope` — hierarchical identity context for memory isolation.
//!
//! Facts and entities are always owned by a `Scope`. Scopes nest:
//! org ⊃ user ⊃ session, with an optional agent dimension at every level.
//! A parent scope can always "see" data owned by child scopes of the same org.

use serde::{Deserialize, Serialize};

/// Hierarchical scope for memory isolation.
///
/// The minimum required field is `org_id`. All other fields are optional and
/// progressively narrow the scope. A `Scope` with only `org_id` is an
/// "org-level" scope; adding `user_id` narrows it to a user; adding
/// `session_id` narrows it further to a single conversation session.
///
/// `agent_id` is orthogonal — it tracks which agent instance wrote or owns
/// a piece of memory, without affecting the containment hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Scope {
    /// Organisation (tenant) identifier. Required; defaults to `"default"`.
    #[serde(default = "default_org")]
    pub org_id: String,

    /// Agent instance identifier (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// End-user identifier (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// Conversation session identifier (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn default_org() -> String {
    "default".to_string()
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            org_id: default_org(),
            agent_id: None,
            user_id: None,
            session_id: None,
        }
    }
}

impl Scope {
    /// Org-level scope — broadest.
    pub fn org(org_id: impl Into<String>) -> Self {
        Self {
            org_id: org_id.into(),
            ..Default::default()
        }
    }

    /// User-level scope — scoped to a specific end-user within an org.
    pub fn user(org_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            org_id: org_id.into(),
            user_id: Some(user_id.into()),
            ..Default::default()
        }
    }

    /// Session-level scope — scoped to a single conversation session.
    pub fn session(
        org_id: impl Into<String>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            org_id: org_id.into(),
            user_id: Some(user_id.into()),
            session_id: Some(session_id.into()),
            ..Default::default()
        }
    }

    /// Full scope — all four fields set.
    pub fn full(
        org_id: impl Into<String>,
        agent_id: impl Into<String>,
        user_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            org_id: org_id.into(),
            agent_id: Some(agent_id.into()),
            user_id: Some(user_id.into()),
            session_id: Some(session_id.into()),
        }
    }

    /// Scope depth: 0 = org-only, 1 = +user, 2 = +session, 3 = all four fields.
    ///
    /// `agent_id` alone does not increase depth; depth measures the narrowing
    /// along the org → user → session hierarchy.
    pub fn depth(&self) -> u8 {
        match (&self.user_id, &self.session_id, &self.agent_id) {
            (None, None, None) => 0,
            (None, None, Some(_)) => 0, // agent_id without user does not deepen
            (Some(_), None, None) => 1,
            (Some(_), None, Some(_)) => 1,
            (Some(_), Some(_), None) => 2,
            (Some(_), Some(_), Some(_)) => 3,
            (None, Some(_), _) => 0, // session without user is degenerate; treat as org
        }
    }

    /// Returns `true` if `self` is a parent (or equal) scope of `other`.
    ///
    /// A scope contains another when:
    /// - Both belong to the same org.
    /// - Every field set in `self` matches the corresponding field in `other`.
    ///   Fields absent in `self` are wildcards.
    pub fn contains(&self, other: &Scope) -> bool {
        if self.org_id != other.org_id {
            return false;
        }
        if let Some(ref a) = self.agent_id {
            if other.agent_id.as_deref() != Some(a.as_str()) {
                return false;
            }
        }
        if let Some(ref u) = self.user_id {
            if other.user_id.as_deref() != Some(u.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = self.session_id {
            if other.session_id.as_deref() != Some(s.as_str()) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_org_only() {
        assert_eq!(Scope::org("acme").depth(), 0);
    }

    #[test]
    fn depth_user() {
        assert_eq!(Scope::user("acme", "alice").depth(), 1);
    }

    #[test]
    fn depth_session() {
        assert_eq!(Scope::session("acme", "alice", "s1").depth(), 2);
    }

    #[test]
    fn depth_full() {
        assert_eq!(Scope::full("acme", "agent-1", "alice", "s1").depth(), 3);
    }

    #[test]
    fn contains_self() {
        let s = Scope::session("acme", "alice", "s1");
        assert!(s.contains(&s));
    }

    #[test]
    fn org_contains_user() {
        let org = Scope::org("acme");
        let user = Scope::user("acme", "alice");
        assert!(org.contains(&user));
        assert!(!user.contains(&org));
    }

    #[test]
    fn different_org_not_contained() {
        let a = Scope::org("acme");
        let b = Scope::org("globex");
        assert!(!a.contains(&b));
    }

    #[test]
    fn serialization_omits_none_fields() {
        let s = Scope::user("acme", "alice");
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"org_id\""));
        assert!(json.contains("\"user_id\""));
        assert!(!json.contains("\"session_id\""));
        assert!(!json.contains("\"agent_id\""));
    }

    #[test]
    fn default_scope_org_is_default() {
        let s = Scope::default();
        assert_eq!(s.org_id, "default");
    }
}
