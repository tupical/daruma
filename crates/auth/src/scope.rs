//! Token scope — what a token may read/write, in which projects.

use serde::{Deserialize, Serialize};
use daruma_shared::ProjectId;

use crate::capability::Capabilities;

/// Which projects a token may touch.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectFilter {
    /// Token is allowed to operate on every project.
    #[default]
    All,
    /// Token is restricted to the listed projects.
    Only { projects: Vec<ProjectId> },
}

impl ProjectFilter {
    /// True if the filter allows operating on `project_id`. A `None` target
    /// (project-agnostic resource) is always allowed.
    pub fn allows(&self, project_id: Option<ProjectId>) -> bool {
        match (self, project_id) {
            (ProjectFilter::All, _) => true,
            (ProjectFilter::Only { .. }, None) => true,
            (ProjectFilter::Only { projects }, Some(p)) => projects.contains(&p),
        }
    }
}

/// Combined project filter + capability mask attached to every token.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenScope {
    #[serde(default)]
    pub projects: ProjectFilter,
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl TokenScope {
    pub fn admin() -> Self {
        Self {
            projects: ProjectFilter::All,
            capabilities: [crate::Capability::Admin].into(),
        }
    }

    /// Default scope for a newly-paired end-user device: read/write access to
    /// tasks, projects, comments, plans, and subscriptions.
    pub fn default_user() -> Self {
        use crate::Capability::*;
        Self {
            projects: ProjectFilter::All,
            capabilities: [
                TaskRead,
                TaskWrite,
                CommentRead,
                CommentWrite,
                ProjectRead,
                ProjectWrite,
                PlanRead,
                PlanWrite,
                RunRead,
                SubscribeTasks,
                SubscribeComments,
                SubscribePlans,
                SubscribeRuns,
                TaskRelationRead,
                DocumentRead,
            ]
            .into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::ProjectId;

    #[test]
    fn project_filter_all_allows_anything() {
        let f = ProjectFilter::All;
        assert!(f.allows(None));
        assert!(f.allows(Some(ProjectId::new())));
    }

    #[test]
    fn project_filter_only_restricts() {
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();
        let f = ProjectFilter::Only { projects: vec![p1] };
        assert!(f.allows(Some(p1)));
        assert!(!f.allows(Some(p2)));
        // project-agnostic events still pass — listing endpoints etc.
        assert!(f.allows(None));
    }

    #[test]
    fn scope_serde_round_trip() {
        let s = TokenScope {
            projects: ProjectFilter::Only {
                projects: vec![ProjectId::new()],
            },
            capabilities: [crate::Capability::TaskRead, crate::Capability::TaskWrite].into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TokenScope = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
