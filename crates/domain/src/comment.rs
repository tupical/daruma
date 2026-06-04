//! Comment entity — a projection of CommentAdded/Edited/Deleted events.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use taskagent_shared::{CommentId, TaskId, Timestamp};

use crate::agent::Actor;

/// Optional semantic classification for a comment (CTM B.1 / §3.8.8).
///
/// Comments without a `kind` (`Option<CommentKind>` = `None`) remain
/// fully valid — every legacy comment in the projection has a NULL
/// `kind` column. The set of variants is closed for now; adding a new
/// kind requires only an extension here (no migration needed because
/// the column is `TEXT NULL`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentKind {
    /// "I'm planning to ..." — declarative scope statement before work begins.
    Intent,
    /// In-flight status / partial-result update.
    Progress,
    /// Concluding summary tied to a `Task`/`Run`'s completion.
    Outcome,
    /// Explicit blocker call-out (orthogonal to `Relation::Blocks` — used when
    /// the blocker isn't another tracked task, e.g. waiting on a human reply).
    Blocker,
    /// Research note / link-dump / external-context capture (used by
    /// `taskagent_research { save_to_task_id }` per §3.8.6).
    Research,
}

impl CommentKind {
    /// Stable string representation, matching the `serde` snake_case form.
    /// This is what we persist in SQLite and emit in MCP responses.
    pub fn as_str(self) -> &'static str {
        match self {
            CommentKind::Intent => "intent",
            CommentKind::Progress => "progress",
            CommentKind::Outcome => "outcome",
            CommentKind::Blocker => "blocker",
            CommentKind::Research => "research",
        }
    }

    /// Canonical list of variants — handy for schema/UI enumeration without
    /// pulling in a derive macro crate.
    pub const ALL: [CommentKind; 5] = [
        CommentKind::Intent,
        CommentKind::Progress,
        CommentKind::Outcome,
        CommentKind::Blocker,
        CommentKind::Research,
    ];
}

impl fmt::Display for CommentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CommentKind {
    type Err = String;

    /// Parses a `CommentKind` from its snake_case canonical form (e.g.
    /// `"research"`). Accepts the PascalCase Rust variant name as a
    /// convenience for hand-typed MCP args (`"Research"`), matching how
    /// the task spec calls the tool. Comparison is case-insensitive so
    /// `"RESEARCH"` works too.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "intent" => Ok(CommentKind::Intent),
            "progress" => Ok(CommentKind::Progress),
            "outcome" => Ok(CommentKind::Outcome),
            "blocker" => Ok(CommentKind::Blocker),
            "research" => Ok(CommentKind::Research),
            other => Err(format!(
                "unknown comment kind: {other:?} (expected one of: intent, progress, outcome, blocker, research)"
            )),
        }
    }
}

/// Canonical comment entity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub task_id: TaskId,
    pub author: Actor,
    pub body: String,
    pub parent_id: Option<CommentId>,
    /// Optional semantic classification — see [`CommentKind`].
    /// `None` for legacy comments and any tool call that didn't supply
    /// the field; serialised as omitted (not `null`) for wire-compactness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CommentKind>,
    pub created_at: Timestamp,
    pub edited_at: Option<Timestamp>,
    pub deleted_at: Option<Timestamp>,
}

impl Comment {
    /// Build a [`Comment`] from a [`NewComment`] input, an actor, and a timestamp.
    pub fn from_new(input: NewComment, actor: Actor, now: Timestamp) -> Self {
        Self {
            id: input.id.unwrap_or_default(),
            task_id: input.task_id,
            author: actor,
            body: input.body,
            parent_id: input.parent_id,
            kind: input.kind,
            created_at: now,
            edited_at: None,
            deleted_at: None,
        }
    }
}

/// Input for creating a comment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewComment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<CommentId>,
    pub task_id: TaskId,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<CommentId>,
    /// Optional [`CommentKind`] classification supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CommentKind>,
}

/// Sparse update for an existing comment.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use taskagent_shared::{CommentId, TaskId};

    #[test]
    fn new_comment_roundtrip_serde() {
        let task_id = TaskId::new();
        let nc = NewComment {
            id: None,
            task_id,
            body: "hello world".to_string(),
            parent_id: None,
            kind: None,
        };
        let json = serde_json::to_string(&nc).unwrap();
        let back: NewComment = serde_json::from_str(&json).unwrap();
        assert_eq!(nc, back);
    }

    #[test]
    fn comment_roundtrip_serde() {
        use taskagent_shared::time;

        let task_id = TaskId::new();
        let actor = crate::agent::Actor::user();
        let now = time::now();
        let nc = NewComment {
            id: Some(CommentId::new()),
            task_id,
            body: "test body".to_string(),
            parent_id: None,
            kind: None,
        };
        let comment = Comment::from_new(nc, actor, now);
        let json = serde_json::to_string(&comment).unwrap();
        let back: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(comment, back);
    }

    #[test]
    fn comment_patch_default_is_empty() {
        let patch = CommentPatch::default();
        assert!(patch.body.is_none());
        let json = serde_json::to_string(&patch).unwrap();
        let back: CommentPatch = serde_json::from_str(&json).unwrap();
        assert_eq!(patch, back);
    }

    // ── CommentKind ────────────────────────────────────────────────────────────

    #[test]
    fn comment_kind_as_str_is_snake_case() {
        assert_eq!(CommentKind::Intent.as_str(), "intent");
        assert_eq!(CommentKind::Progress.as_str(), "progress");
        assert_eq!(CommentKind::Outcome.as_str(), "outcome");
        assert_eq!(CommentKind::Blocker.as_str(), "blocker");
        assert_eq!(CommentKind::Research.as_str(), "research");
    }

    #[test]
    fn comment_kind_display_matches_as_str() {
        for k in CommentKind::ALL {
            assert_eq!(format!("{k}"), k.as_str());
        }
    }

    #[test]
    fn comment_kind_from_str_canonical() {
        for k in CommentKind::ALL {
            let parsed: CommentKind = k.as_str().parse().unwrap();
            assert_eq!(parsed, k);
        }
    }

    #[test]
    fn comment_kind_from_str_accepts_pascal_case_and_is_case_insensitive() {
        // Task spec uses kind="Research"; we accept that too.
        assert_eq!(
            "Research".parse::<CommentKind>().unwrap(),
            CommentKind::Research
        );
        assert_eq!(
            "BLOCKER".parse::<CommentKind>().unwrap(),
            CommentKind::Blocker
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(
            "  intent  ".parse::<CommentKind>().unwrap(),
            CommentKind::Intent
        );
    }

    #[test]
    fn comment_kind_from_str_rejects_unknown() {
        let err = "bogus".parse::<CommentKind>().unwrap_err();
        assert!(err.contains("unknown comment kind"));
    }

    #[test]
    fn comment_kind_serde_roundtrip_snake_case() {
        for k in CommentKind::ALL {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            let back: CommentKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn comment_kind_none_is_omitted_from_json() {
        let task_id = TaskId::new();
        let nc = NewComment {
            id: None,
            task_id,
            body: "x".to_string(),
            parent_id: None,
            kind: None,
        };
        let json = serde_json::to_string(&nc).unwrap();
        assert!(
            !json.contains("kind"),
            "kind should be omitted when None, got: {json}"
        );
    }

    #[test]
    fn from_new_propagates_kind() {
        use taskagent_shared::time;

        let task_id = TaskId::new();
        let nc = NewComment {
            id: None,
            task_id,
            body: "did the thing".to_string(),
            parent_id: None,
            kind: Some(CommentKind::Outcome),
        };
        let c = Comment::from_new(nc, crate::agent::Actor::user(), time::now());
        assert_eq!(c.kind, Some(CommentKind::Outcome));
    }
}
