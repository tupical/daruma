//! Common primitives shared across all taskagent crates.
//!
//! Intentionally dependency-free of business logic — only IDs, time
//! helpers, and the top-level error live here.

pub mod error;
pub mod ids;
pub mod path_lease;
pub mod time;

pub use error::CoreError;
pub use ids::{
    ActivityId, AgentId, AgentSessionId, AiOpId, ArtifactId, ArtifactRelationId, AuditFindingId,
    CommentId, DeviceId, DocumentId, EventId, EvidenceId, PlanId, ProjectId, RelationId, RuleId,
    RunId, RunNoteId, SessionArtifactId, TaskId, TokenId, VersionId, WebhookDeliveryId, WebhookId,
    WorkLeaseId, WorkUnitId,
};
pub use path_lease::{normalize_lease_path, paths_overlap};
pub use time::Timestamp;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;
