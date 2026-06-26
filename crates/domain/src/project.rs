use serde::{Deserialize, Serialize};
use daruma_shared::{time, ProjectId, Timestamp};

pub const DEFAULT_TENANT_ID: &str = "self-hosted";

/// Build a URL-safe slug from a project title.
pub fn slugify_title(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "project".into();
    }
    if slug.len() > 64 {
        slug.truncate(64);
        slug = slug.trim_end_matches('-').to_string();
    }
    slug
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// `#[serde(default)]` so `project_created` events persisted before the
    /// slug field existed (pre-migration-0018) still deserialise during event
    /// replay / workspace-graph catch-up. The canonical `projects` table is
    /// backfilled by the migration; this default only affects historical
    /// event payloads, where an empty slug is harmless graph metadata.
    #[serde(default)]
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    #[serde(default)]
    pub triage_enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Project {
    pub fn new(title: impl Into<String>, description: Option<String>) -> Self {
        let title = title.into();
        let slug = slugify_title(&title);
        Self::new_with_slug(title, slug, description)
    }

    pub fn new_with_slug(
        title: impl Into<String>,
        slug: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        let now = time::now();
        Self {
            id: ProjectId::new(),
            tenant_id: Some(DEFAULT_TENANT_ID.to_string()),
            slug: slug.into(),
            title: title.into(),
            description,
            triage_enabled: false,
            created_at: now,
            updated_at: now,
        }
    }
}
