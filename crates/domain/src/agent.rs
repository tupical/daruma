use daruma_shared::{time, AgentId, ProjectId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

use crate::task::Priority;

/// Who initiated a command/event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    /// A human user — directly through a client, or through an agent that
    /// acts under that user's token. `id` is the authenticated principal
    /// (the token's `agent_id`, or the hosting platform's account id) and
    /// `name` an optional display name (e.g. e-mail); both are absent for
    /// anonymous/legacy records, so `{"kind":"user"}` keeps round-tripping.
    User {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<AgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// MCP client the call came through (see [`Actor::Agent::client`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    /// An AI agent. The `name` is a free-form identifier (e.g.
    /// "responses-gpt-4.1" or "local-parser"). `client` is the MCP client the
    /// agent called through (`clientInfo.name` of `initialize`, else the
    /// User-Agent product, e.g. `codex-mcp-client`) — attribution for
    /// per-client compliance measurement; absent for non-MCP and legacy
    /// records, so old `actor_json` keeps round-tripping unchanged.
    Agent {
        id: AgentId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
}

impl Default for Actor {
    fn default() -> Self {
        Self::User {
            id: None,
            name: None,
            client: None,
        }
    }
}

impl Actor {
    /// Anonymous user (no principal known).
    pub fn user() -> Self {
        Self::default()
    }

    /// Identified user: the principal that authenticated the call.
    pub fn user_with_id(id: AgentId) -> Self {
        Self::User {
            id: Some(id),
            name: None,
            client: None,
        }
    }

    /// Authenticated principal behind this actor, if any.
    pub fn principal_id(&self) -> Option<AgentId> {
        match self {
            Self::User { id, .. } => *id,
            Self::Agent { id, .. } => Some(*id),
        }
    }

    pub fn agent(name: impl Into<String>) -> Self {
        Self::Agent {
            id: AgentId::new(),
            name: name.into(),
            client: None,
        }
    }

    /// MCP client attributed to this actor, if any.
    pub fn client(&self) -> Option<&str> {
        match self {
            Self::User { client, .. } | Self::Agent { client, .. } => client.as_deref(),
        }
    }

    /// Attribute `client` unless the actor already names one.
    pub fn or_client(mut self, fallback: Option<&str>) -> Self {
        let (Self::User { client, .. } | Self::Agent { client, .. }) = &mut self;
        if client.is_none() {
            *client = fallback.map(str::to_string);
        }
        self
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// What the agent suggests. Suggestions do **not** mutate state directly —
/// they become commands when accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentActionKind {
    SuggestTask {
        title: String,
        reason: String,
    },
    SuggestSplit {
        parent: TaskId,
        subtasks: Vec<String>,
    },
    SuggestPriority {
        task: TaskId,
        suggested: Priority,
        reason: String,
    },
    SummarizeProject {
        project_id: ProjectId,
        summary: String,
    },
    SuggestNextAction {
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentAction {
    pub agent_name: String,
    pub kind: AgentActionKind,
    pub created_at: Timestamp,
}

impl AgentAction {
    pub fn new(agent_name: impl Into<String>, kind: AgentActionKind) -> Self {
        Self {
            agent_name: agent_name.into(),
            kind,
            created_at: time::now(),
        }
    }
}

#[cfg(test)]
mod actor_tests {
    use super::*;

    #[test]
    fn anonymous_user_round_trips_as_bare_kind() {
        let json = serde_json::to_value(Actor::user()).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "user"}));
        let parsed: Actor = serde_json::from_value(serde_json::json!({"kind": "user"})).unwrap();
        assert_eq!(parsed, Actor::user());
        assert_eq!(Actor::default(), Actor::user());
        assert!(parsed.principal_id().is_none());
    }

    #[test]
    fn identified_user_carries_principal_and_optional_name() {
        let id = AgentId::new();
        let actor = Actor::User {
            id: Some(id),
            name: Some("owner@example.com".into()),
            client: None,
        };
        let json = serde_json::to_value(&actor).unwrap();
        assert_eq!(json["kind"], "user");
        assert_eq!(json["id"], serde_json::json!(id.as_uuid().to_string()));
        assert_eq!(json["name"], "owner@example.com");
        let back: Actor = serde_json::from_value(json).unwrap();
        assert_eq!(back, actor);
        assert_eq!(back.principal_id(), Some(id));
        assert!(!back.is_agent());
        // Legacy/foreign payloads with an id but no name stay a user.
        let only_id: Actor = serde_json::from_value(
            serde_json::json!({"kind": "user", "id": id.as_uuid().to_string()}),
        )
        .unwrap();
        assert_eq!(only_id, Actor::user_with_id(id));
    }

    #[test]
    fn agent_client_is_optional_and_round_trips() {
        let id = AgentId::new();
        let legacy =
            serde_json::json!({"kind": "agent", "id": id.as_uuid().to_string(), "name": "mcp"});
        let parsed: Actor = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(parsed.client(), None);
        // No client → serialized shape is unchanged (no `client` key).
        assert_eq!(serde_json::to_value(&parsed).unwrap(), legacy);

        let actor = Actor::Agent {
            id,
            name: "mcp".into(),
            client: Some("codex-mcp-client".into()),
        };
        let json = serde_json::to_value(&actor).unwrap();
        assert_eq!(json["client"], "codex-mcp-client");
        let back: Actor = serde_json::from_value(json).unwrap();
        assert_eq!(back.client(), Some("codex-mcp-client"));
        assert_eq!(back, actor);

        // `or_client` fills only a missing client.
        assert_eq!(
            Actor::user().or_client(Some("cursor")).client(),
            Some("cursor")
        );
        assert_eq!(
            back.or_client(Some("cursor")).client(),
            Some("codex-mcp-client")
        );
        assert_eq!(Actor::user().or_client(None), Actor::user());
    }
}
