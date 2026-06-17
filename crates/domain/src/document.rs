//! Document entity — markdown artefact attached to a Project.
//!
//! Spec: `.omc/plans/document-pr1-spec.md` §1.
//!
//! A `Document` is a free-form markdown blob that belongs to a project. Two
//! default documents (`Interview` and `Human Log`) are auto-created on
//! `Command::CreateProject` so every project has a known place to store
//! requirements-gathering notes and a working log. Additional documents may
//! be created freely — `kind` is *not* unique per project.

use serde::{Deserialize, Serialize};
use taskagent_shared::{DocumentId, ProjectId, Timestamp};

/// Discriminator for default document slots.
///
/// Today this is a closed set: every project gets exactly one default
/// `Interview` and one default `Human Log` created by the
/// `Command::CreateProject` handler. Users may create additional documents
/// of either kind via `Command::CreateDocument`.
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
    use taskagent_shared::{time, DocumentId, ProjectId};

    fn make_doc(kind: DocumentKind) -> Document {
        let now = time::now();
        Document {
            id: DocumentId::new(),
            project_id: ProjectId::new(),
            kind,
            title: "Interview".to_string(),
            content: "# Hello\n\nWorld".to_string(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            last_read_at: None,
            last_read_by: None,
            read_count: 0,
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
        };
        let doc = new_doc.into_document(DocumentId::new(), time::now());
        assert_eq!(doc.content, "# Header\n\nbody");
    }
}
