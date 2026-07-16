//! Document entity — markdown artefact attached to a Project.
//!
//! Spec: `.omc/plans/document-pr1-spec.md` §1.
//!
//! A `Document` is a free-form markdown blob that belongs to a project — the
//! execution layer's structured task-artifact store. Documents are created
//! explicitly via `Command::CreateDocument` (`kind` is *not* unique per
//! project); the core no longer auto-seeds any default documents on
//! `Command::CreateProject`. The narrative `Interview` / `Human Log` kinds
//! exist for product layers (Intake / Sensemaking) that opt into them, but
//! seeding them is product behaviour, not an execution-core default.

use daruma_shared::{DocumentId, ProjectId, TaskId, Timestamp};
use serde::{Deserialize, Serialize};

/// Discriminator for document kinds.
///
/// Today this is a closed set of two narrative kinds (`Interview`,
/// `Human Log`). These are *not* auto-created by the core; callers create
/// documents of either kind explicitly via `Command::CreateDocument`, and a
/// project may have zero, one, or many of each.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Interview,
    HumanLog,
}

impl DocumentKind {
    /// Stable string representation matching the `serde` snake_case form.
    /// Used for SQL persistence and stable comparison in tests / logs.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentKind::Interview => "interview",
            DocumentKind::HumanLog => "human_log",
        }
    }
}

/// Lifecycle status of a document (OSS task 019eb65b; vision.md rule 9).
///
/// The minimum viable slice of the target taxonomy
/// (`draft/active/used/needs_review/accepted/outdated/replaced/archived`);
/// stored as TEXT so extending the set later is additive. `Archived` is kept
/// coherent with `Document::archived_at` by the projector: entering
/// `Archived` stamps `archived_at`, leaving it clears the stamp.
///
/// `Frozen` and `Replaced` (task 019f6ad2; canon daruma invariant 5, "живой
/// документ ⇔ живой якорь") extend the taxonomy additively for the
/// document-task anchor barrier: `Frozen` is the terminal state a document
/// lands in when its anchor task completes (`Done`) — still readable, no
/// longer live/editable-by-default. `Replaced` is reserved for the target
/// taxonomy's "superseded by a newer document" state; nothing in the core
/// emits it yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    Draft,
    /// The pre-lifecycle implicit state — existing documents and new
    /// documents created without an explicit status land here, so the
    /// old "document = live markdown blob" behaviour is unchanged.
    #[default]
    Active,
    Outdated,
    Archived,
    /// The task this document is an artifact of reached `Done`. Distinct
    /// from `Archived` (which is reserved for `Cancelled` anchors and
    /// explicit archiving): a frozen document is a completed artifact, not
    /// a discarded one.
    Frozen,
    /// Superseded by a newer document. Reserved for the target taxonomy;
    /// nothing currently emits this status.
    Replaced,
}

impl DocumentStatus {
    /// Stable string form matching the `serde` snake_case representation.
    /// Used for SQL persistence and stable comparison in tests / logs.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentStatus::Draft => "draft",
            DocumentStatus::Active => "active",
            DocumentStatus::Outdated => "outdated",
            DocumentStatus::Archived => "archived",
            DocumentStatus::Frozen => "frozen",
            DocumentStatus::Replaced => "replaced",
        }
    }
}

/// A markdown artefact owned by a project.
///
/// `archived_at` follows the same convention as `Plan::archived_at`: `None`
/// = active, `Some(_)` = soft-deleted but still readable for audit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    pub project_id: ProjectId,
    pub kind: DocumentKind,
    pub title: String,
    /// Raw markdown body.
    pub content: String,
    /// Lifecycle status (migration 0042). Defaults to [`DocumentStatus::Active`]
    /// so documents from before the lifecycle existed keep their semantics.
    #[serde(default)]
    pub status: DocumentStatus,
    /// The task this document is an artifact of (vision.md: "не документы, а
    /// артефакты задачи"). `None` = project-level document, the pre-lifecycle
    /// shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// What triggered the document's creation (free-form, e.g.
    /// `"before_start_rule"`, `"handoff"`). Metadata for Cloud rules 6–9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_kind: Option<String>,
    /// Who/what is expected to consume the document (free-form, e.g.
    /// `"executor_agent"`, `"reviewer"`). Metadata for Cloud rules 6–9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Timestamp>,
    /// Passive read-tracking (migration 0039). `None` = never read. Updated in
    /// place by `doc_get`, throttled per (document, actor); *not* event-sourced.
    /// Distinct from the explicit, immutable evidence `document_read_ack`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at: Option<Timestamp>,
    /// Who last read it (the `ActorRef` "user"|"agent" kind string, or an agent
    /// display name). `None` = never read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_by: Option<String>,
    /// Monotonic count of throttled read events; `0` = never read.
    #[serde(default)]
    pub read_count: u64,
}

/// Input for creating a new Document.
///
/// `id` is optional: callers that need to reference the document in a
/// follow-up event in the same batch can pre-allocate; otherwise the
/// command handler allocates a fresh `DocumentId`. `content` defaults to
/// empty when not supplied.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NewDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<DocumentId>,
    pub project_id: ProjectId,
    pub kind: DocumentKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Initial lifecycle status; `None` = [`DocumentStatus::Active`] (the
    /// pre-lifecycle behaviour, kept as the default for compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DocumentStatus>,
    /// Task the document is created as an artifact of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer: Option<String>,
}

impl NewDocument {
    /// Materialise into a full [`Document`] given a pre-allocated id and
    /// wall-clock `now`. `archived_at` is always `None` for newly created
    /// documents.
    pub fn into_document(self, id: DocumentId, now: Timestamp) -> Document {
        Document {
            id,
            project_id: self.project_id,
            kind: self.kind,
            title: self.title,
            content: self.content.unwrap_or_default(),
            status: self.status.unwrap_or_default(),
            task_id: self.task_id,
            trigger_kind: self.trigger_kind,
            consumer: self.consumer,
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_read_at: None,
            last_read_by: None,
            read_count: 0,
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_shared::{time, DocumentId, ProjectId};

    fn make_doc(kind: DocumentKind) -> Document {
        let now = time::now();
        Document {
            id: DocumentId::new(),
            project_id: ProjectId::new(),
            kind,
            title: "Interview".to_string(),
            content: "# Hello\n\nWorld".to_string(),
            status: DocumentStatus::Active,
            task_id: None,
            trigger_kind: None,
            consumer: None,
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_read_at: None,
            last_read_by: None,
            read_count: 0,
        }
    }

    #[test]
    fn document_status_as_str() {
        assert_eq!(DocumentStatus::Draft.as_str(), "draft");
        assert_eq!(DocumentStatus::Active.as_str(), "active");
        assert_eq!(DocumentStatus::Outdated.as_str(), "outdated");
        assert_eq!(DocumentStatus::Archived.as_str(), "archived");
        assert_eq!(DocumentStatus::Frozen.as_str(), "frozen");
        assert_eq!(DocumentStatus::Replaced.as_str(), "replaced");
    }

    #[test]
    fn document_status_roundtrip_serde() {
        for status in [
            DocumentStatus::Draft,
            DocumentStatus::Active,
            DocumentStatus::Outdated,
            DocumentStatus::Archived,
            DocumentStatus::Frozen,
            DocumentStatus::Replaced,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: DocumentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn document_kind_as_str() {
        assert_eq!(DocumentKind::Interview.as_str(), "interview");
        assert_eq!(DocumentKind::HumanLog.as_str(), "human_log");
    }

    #[test]
    fn document_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&DocumentKind::Interview).unwrap(),
            "\"interview\""
        );
        assert_eq!(
            serde_json::to_string(&DocumentKind::HumanLog).unwrap(),
            "\"human_log\""
        );
    }

    #[test]
    fn document_kind_roundtrip_serde() {
        for kind in [DocumentKind::Interview, DocumentKind::HumanLog] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: DocumentKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn document_roundtrip_serde_interview() {
        let doc = make_doc(DocumentKind::Interview);
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn document_roundtrip_serde_human_log() {
        let doc = make_doc(DocumentKind::HumanLog);
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn document_roundtrip_serde_archived() {
        let mut doc = make_doc(DocumentKind::HumanLog);
        doc.archived_at = Some(time::now());
        let json = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn document_archived_omitted_when_none() {
        let doc = make_doc(DocumentKind::Interview);
        let json = serde_json::to_string(&doc).unwrap();
        assert!(
            !json.contains("archived_at"),
            "archived_at should be omitted when None, got: {json}"
        );
    }

    #[test]
    fn new_document_roundtrip_serde() {
        let new_doc = NewDocument {
            id: Some(DocumentId::new()),
            project_id: ProjectId::new(),
            kind: DocumentKind::Interview,
            title: "Interview".to_string(),
            content: Some("body".to_string()),
            status: Some(DocumentStatus::Draft),
            task_id: None,
            trigger_kind: Some("before_start_rule".to_string()),
            consumer: Some("reviewer".to_string()),
        };
        let json = serde_json::to_string(&new_doc).unwrap();
        let back: NewDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(new_doc, back);
    }

    #[test]
    fn new_document_minimal_roundtrip_serde() {
        let new_doc = NewDocument {
            id: None,
            project_id: ProjectId::new(),
            kind: DocumentKind::HumanLog,
            title: "Human Log".to_string(),
            content: None,
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        };
        let json = serde_json::to_string(&new_doc).unwrap();
        assert!(!json.contains("\"id\""));
        assert!(!json.contains("\"content\""));
        let back: NewDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(new_doc, back);
    }

    #[test]
    fn new_document_into_document_defaults_content_to_empty() {
        let project_id = ProjectId::new();
        let now = time::now();
        let new_doc = NewDocument {
            id: None,
            project_id,
            kind: DocumentKind::Interview,
            title: "Interview".to_string(),
            content: None,
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        };
        let id = DocumentId::new();
        let doc = new_doc.into_document(id, now);
        assert_eq!(doc.id, id);
        assert_eq!(doc.project_id, project_id);
        assert_eq!(doc.kind, DocumentKind::Interview);
        assert_eq!(doc.title, "Interview");
        assert_eq!(doc.content, "");
        assert_eq!(doc.created_at, now);
        assert_eq!(doc.updated_at, now);
        assert!(doc.archived_at.is_none());
    }

    #[test]
    fn new_document_into_document_preserves_content() {
        let new_doc = NewDocument {
            id: None,
            project_id: ProjectId::new(),
            kind: DocumentKind::HumanLog,
            title: "Human Log".to_string(),
            content: Some("# Header\n\nbody".to_string()),
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        };
        let doc = new_doc.into_document(DocumentId::new(), time::now());
        assert_eq!(doc.content, "# Header\n\nbody");
    }
}
