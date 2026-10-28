//! Per-request authentication context inserted by middleware.

use daruma_domain::Actor;
use daruma_shared::{AgentId, DeviceId, TokenId};
use serde::{Deserialize, Serialize};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub tenant_id: Option<String>,
    pub rate_limit_per_min: u32,
    pub scope: TokenScope,
    /// Kind of token that produced this context. Used by [`AuthContext::actor`]
    /// to derive the correct [`Actor`] for event attribution.
    pub token_kind: TokenKind,
    /// Person the token acts for, when an embedding host knows it better than
    /// the token's own `agent_id` (see [`HostPrincipal`]). Attribution only:
    /// claims and leases stay keyed by `agent_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<AgentId>,
    /// Self-reported MCP client (`X-Daruma-Client`, see
    /// [`CLIENT_HEADER`]) stamped onto [`AuthContext::actor`]. Attribution
    /// only — never an authorization input. Set per request, never cached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Request header naming the MCP client a call came through (the hosted MCP
/// gateway copies `clientInfo.name` / the User-Agent product into it).
pub const CLIENT_HEADER: &str = "x-daruma-client";

/// Longest client name kept; longer values are truncated.
pub const CLIENT_MAX_CHARS: usize = 64;

/// Normalise a raw client name: trimmed, control characters dropped,
/// truncated to [`CLIENT_MAX_CHARS`]; `None` when nothing is left.
pub fn sanitize_client(raw: &str) -> Option<String> {
    let name: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(CLIENT_MAX_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Principal asserted by an embedding host (the cloud gateway: the account
/// behind the token). Request extensions are set only by in-process code,
/// never by clients, so the auth middleware copies it into
/// [`AuthContext::principal_id`] as trusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostPrincipal(pub AgentId);

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
    /// - `TokenKind::Pat | TokenKind::Svc | TokenKind::Usr | TokenKind::License` →
    ///   `Actor::User { id: principal_id or agent_id }` — the token's principal
    ///   is the person (or the service acting for them), so the journal can
    ///   answer "who".
    pub fn actor(&self) -> Actor {
        match self.token_kind {
            TokenKind::Bot => Actor::Agent {
                id: self.agent_id,
                name: format!("bot.{}", self.agent_id),
                client: None,
            },
            TokenKind::Pat | TokenKind::Svc | TokenKind::Usr | TokenKind::License => {
                Actor::user_with_id(self.principal_id.unwrap_or(self.agent_id))
            }
        }
        .or_client(self.client.as_deref())
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
