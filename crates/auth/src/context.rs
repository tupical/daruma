//! Per-request authentication context inserted by middleware.

use serde::{Deserialize, Serialize};
use taskagent_domain::Actor;
use taskagent_shared::{AgentId, TokenId};

use crate::capability::Capability;
use crate::scope::TokenScope;
use crate::token::TokenKind;

/// Cheap-to-clone snapshot of the authenticated principal. Inserted into
/// request extensions by the auth middleware; handlers extract it to gate
/// access via [`AuthContext::require`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthContext {
    pub agent_id: AgentId,
    pub token_id: TokenId,
    pub tenant_id: Option<String>,
    pub rate_limit_per_min: u32,
    pub scope: TokenScope,
    /// Kind of token that produced this context. Used by [`AuthContext::actor`]
    /// to derive the correct [`Actor`] for event attribution.
    pub token_kind: TokenKind,
}

impl AuthContext {
    /// Return `Ok(())` if the token's scope holds `cap`. Used at the top of
    /// each handler to gate writes/reads consistently.
    pub fn require(&self, cap: Capability) -> Result<(), MissingCapability> {
        if self.scope.capabilities.has(cap) {
            Ok(())
        } else {
            Err(MissingCapability { needed: cap })
        }
    }

    /// Derive the [`Actor`] that should be attributed to commands/events
    /// dispatched on behalf of this token.
    ///
    /// - `TokenKind::Bot` → `Actor::Agent { id: agent_id, name: "bot.<agent_id>" }`
    /// - `TokenKind::Pat | TokenKind::Svc | TokenKind::Usr | TokenKind::License` → `Actor::User`
    pub fn actor(&self) -> Actor {
        match self.token_kind {
            TokenKind::Bot => Actor::Agent {
                id: self.agent_id,
                name: format!("bot.{}", self.agent_id),
            },
            TokenKind::Pat | TokenKind::Svc | TokenKind::Usr | TokenKind::License => Actor::User,
        }
    }
}

/// Returned by [`AuthContext::require`] when the token lacks a capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingCapability {
    pub needed: Capability,
}

impl std::fmt::Display for MissingCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token is missing capability: {}", self.needed.name())
    }
}

impl std::error::Error for MissingCapability {}
