//! Capability bit-flag set.
//!
//! Stored on each [`ApiToken`](crate::ApiToken) as a `u32`. Handlers gate
//! their effects via [`AuthContext::require`](crate::AuthContext::require).

use serde::{Deserialize, Serialize};

/// Single capability bit. The discriminant *is* the bit value, so combining
/// capabilities into a [`Capabilities`] mask is just a bitwise-OR.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Capability {
    TaskRead = 1 << 0,
    TaskWrite = 1 << 1,
    CommentRead = 1 << 2,
    CommentWrite = 1 << 3,
    AgentDispatch = 1 << 4,
    ProjectRead = 1 << 5,
    ProjectWrite = 1 << 6,
    SubscribeTasks = 1 << 7,
    SubscribeComments = 1 << 8,
    SubscribeAgentStatus = 1 << 9,
    WebhookRead = 1 << 10,
    WebhookWrite = 1 << 11,
    TokenRead = 1 << 12,
    TokenWrite = 1 << 13,
    PlanRead = 1 << 14,
    PlanWrite = 1 << 15,
    RunRead = 1 << 16,
    RunWrite = 1 << 17,
    /// Subscribe to plan lifecycle and mutation events (`Channel::Plans`).
    SubscribePlans = 1 << 18,
    /// Subscribe to run lifecycle, agent-session, and signal events (`Channel::Runs`).
    SubscribeRuns = 1 << 19,
    /// Read task relations (REST `GET /v1/tasks/{id}/relations`).
    TaskRelationRead = 1 << 20,
    /// Create or delete task relations (REST `POST/DELETE /v1/relations`).
    TaskRelationWrite = 1 << 21,
    /// Read documents (REST `GET /v1/documents/*`, `GET /v1/projects/.../documents`).
    DocumentRead = 1 << 22,
    /// Create / mutate / archive documents (REST `POST/PATCH /v1/documents/*`).
    DocumentWrite = 1 << 23,
    /// Wildcard — grants every other capability implicitly. Used by the
    /// bootstrap token and admin-issued service tokens.
    Admin = 1 << 31,
}

impl Capability {
    pub fn as_bit(self) -> u32 {
        self as u32
    }

    /// Stable kebab-case name (used in logs and error messages).
    pub fn name(self) -> &'static str {
        match self {
            Capability::TaskRead => "task:read",
            Capability::TaskWrite => "task:write",
            Capability::CommentRead => "comment:read",
            Capability::CommentWrite => "comment:write",
            Capability::AgentDispatch => "agent:dispatch",
            Capability::ProjectRead => "project:read",
            Capability::ProjectWrite => "project:write",
            Capability::SubscribeTasks => "subscribe:tasks",
            Capability::SubscribeComments => "subscribe:comments",
            Capability::SubscribeAgentStatus => "subscribe:agent_status",
            Capability::WebhookRead => "webhook:read",
            Capability::WebhookWrite => "webhook:write",
            Capability::TokenRead => "token:read",
            Capability::TokenWrite => "token:write",
            Capability::PlanRead => "plan:read",
            Capability::PlanWrite => "plan:write",
            Capability::RunRead => "run:read",
            Capability::RunWrite => "run:write",
            Capability::SubscribePlans => "subscribe:plans",
            Capability::SubscribeRuns => "subscribe:runs",
            Capability::TaskRelationRead => "task_relation:read",
            Capability::TaskRelationWrite => "task_relation:write",
            Capability::DocumentRead => "document:read",
            Capability::DocumentWrite => "document:write",
            Capability::Admin => "admin",
        }
    }
}

/// Bitmask over [`Capability`]. Wraps a `u32` for stable JSON encoding.
///
/// Holding [`Capability::Admin`] implies all other capabilities.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub fn as_bits(self) -> u32 {
        self.0
    }

    /// True if the mask holds the given capability, either explicitly or
    /// transitively via [`Capability::Admin`].
    pub fn has(self, cap: Capability) -> bool {
        (self.0 & Capability::Admin.as_bit()) != 0 || (self.0 & cap.as_bit()) != 0
    }

    /// Add a capability in-place.
    pub fn grant(&mut self, cap: Capability) {
        self.0 |= cap.as_bit();
    }

    /// Remove a capability in-place.
    pub fn revoke(&mut self, cap: Capability) {
        self.0 &= !cap.as_bit();
    }
}

impl<I: IntoIterator<Item = Capability>> From<I> for Capabilities {
    fn from(caps: I) -> Self {
        let mut out = Capabilities::empty();
        for c in caps {
            out.grant(c);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_nothing() {
        let caps = Capabilities::empty();
        assert!(!caps.has(Capability::TaskRead));
    }

    #[test]
    fn grant_revoke_round_trip() {
        let mut caps = Capabilities::empty();
        caps.grant(Capability::TaskWrite);
        assert!(caps.has(Capability::TaskWrite));
        caps.revoke(Capability::TaskWrite);
        assert!(!caps.has(Capability::TaskWrite));
    }

    #[test]
    fn admin_implies_everything() {
        let caps: Capabilities = [Capability::Admin].into();
        assert!(caps.has(Capability::TaskRead));
        assert!(caps.has(Capability::WebhookWrite));
        assert!(caps.has(Capability::TokenWrite));
        // New relation capabilities also implied by Admin.
        assert!(caps.has(Capability::TaskRelationRead));
        assert!(caps.has(Capability::TaskRelationWrite));
    }

    #[test]
    fn task_relation_caps_explicit_grant() {
        let mut caps = Capabilities::empty();
        assert!(!caps.has(Capability::TaskRelationRead));
        assert!(!caps.has(Capability::TaskRelationWrite));
        caps.grant(Capability::TaskRelationRead);
        assert!(caps.has(Capability::TaskRelationRead));
        assert!(!caps.has(Capability::TaskRelationWrite));
        caps.grant(Capability::TaskRelationWrite);
        assert!(caps.has(Capability::TaskRelationWrite));
    }

    #[test]
    fn task_relation_cap_names() {
        assert_eq!(Capability::TaskRelationRead.name(), "task_relation:read");
        assert_eq!(Capability::TaskRelationWrite.name(), "task_relation:write");
    }

    #[test]
    fn task_relation_bits_do_not_overlap() {
        let read_bit = Capability::TaskRelationRead.as_bit();
        let write_bit = Capability::TaskRelationWrite.as_bit();
        assert_ne!(read_bit, write_bit);
        assert_eq!(read_bit & write_bit, 0);
    }

    #[test]
    fn serde_round_trip() {
        let caps: Capabilities = [
            Capability::TaskRead,
            Capability::TaskWrite,
            Capability::CommentRead,
        ]
        .into();
        let json = serde_json::to_string(&caps).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }
}
