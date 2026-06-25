//! Webhook domain types — also the wire format for the admin endpoints.

use serde::{Deserialize, Serialize};
use daruma_auth::ProjectFilter;
use daruma_shared::{time, Timestamp, WebhookId};

/// A persisted outbound webhook subscription.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Webhook {
    pub id: WebhookId,
    pub url: String,
    /// Shared secret used to sign payloads with HMAC-SHA256. Never echoed
    /// back from the admin endpoint after creation (handlers must redact).
    pub secret: String,
    /// Event-kind allow-list. Empty means "deliver every event".
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub project_filter: ProjectFilter,
    pub is_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// §3.7.5 — pre-assembled context the dispatcher should attach to the
    /// outbound payload. Each entry is an opaque key (e.g. `"parent_plan"`,
    /// `"project"`, `"task"`); unknown keys are ignored at delivery time so
    /// adding a new key never requires a schema change. Empty (the default)
    /// means "deliver the raw envelope unchanged" — preserves backward
    /// compatibility with every pre-§3.7.5 subscription.
    #[serde(default)]
    pub enrich: Vec<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Webhook {
    /// True if this webhook subscribes to the given event kind.
    pub fn matches_kind(&self, kind: &str) -> bool {
        self.events.is_empty() || self.events.iter().any(|k| k == kind)
    }
}

/// Input for `POST /v1/webhooks`. `id` is server-assigned unless caller
/// supplied one. `secret` is required and never recoverable later.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewWebhook {
    #[serde(default)]
    pub id: Option<WebhookId>,
    pub url: String,
    pub secret: String,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub project_filter: ProjectFilter,
    #[serde(default = "default_active")]
    pub is_active: bool,
    #[serde(default)]
    pub description: Option<String>,
    /// §3.7.5 — opt-in enrichment keys. Defaults to empty for backward
    /// compatibility; existing callers that POST without `enrich` keep their
    /// pre-§3.7.5 behaviour.
    #[serde(default)]
    pub enrich: Vec<String>,
}

fn default_active() -> bool {
    true
}

impl NewWebhook {
    pub fn into_webhook(self) -> Webhook {
        let now = time::now();
        Webhook {
            id: self.id.unwrap_or_default(),
            url: self.url,
            secret: self.secret,
            events: self.events,
            project_filter: self.project_filter,
            is_active: self.is_active,
            description: self.description,
            enrich: self.enrich,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Sparse patch for `PATCH /v1/webhooks/{id}`. `None` outer = keep.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WebhookPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_filter: Option<ProjectFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Option<String>>,
    /// §3.7.5 — replace the enrich list. `None` keeps the existing list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrich: Option<Vec<String>>,
}

impl WebhookPatch {
    pub fn apply(&self, w: &mut Webhook) {
        if let Some(u) = &self.url {
            w.url = u.clone();
        }
        if let Some(s) = &self.secret {
            w.secret = s.clone();
        }
        if let Some(e) = &self.events {
            w.events = e.clone();
        }
        if let Some(p) = &self.project_filter {
            w.project_filter = p.clone();
        }
        if let Some(active) = self.is_active {
            w.is_active = active;
        }
        if let Some(d) = &self.description {
            w.description = d.clone();
        }
        if let Some(en) = &self.enrich {
            w.enrich = en.clone();
        }
        w.updated_at = time::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_kind_wildcard_when_empty() {
        let now = time::now();
        let w = Webhook {
            id: WebhookId::new(),
            url: "https://example".into(),
            secret: "s".into(),
            events: vec![],
            project_filter: ProjectFilter::All,
            is_active: true,
            description: None,
            enrich: vec![],
            created_at: now,
            updated_at: now,
        };
        assert!(w.matches_kind("task_created"));
        assert!(w.matches_kind("anything"));
    }

    #[test]
    fn matches_kind_filters_when_set() {
        let now = time::now();
        let w = Webhook {
            id: WebhookId::new(),
            url: "https://example".into(),
            secret: "s".into(),
            events: vec!["task_reopened".into(), "task_commented".into()],
            project_filter: ProjectFilter::All,
            is_active: true,
            description: None,
            enrich: vec![],
            created_at: now,
            updated_at: now,
        };
        assert!(w.matches_kind("task_reopened"));
        assert!(!w.matches_kind("task_created"));
    }
}
