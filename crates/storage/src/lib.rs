//! SQLite-backed persistence for Daruma.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use daruma_storage::{Db, SqliteEventStore, TaskRepo, ProjectRepo};
//!
//! # async fn example() -> daruma_shared::Result<()> {
//! let db = Db::open("daruma.db").await?;
//! db.migrate().await?;
//!
//! let pool = db.pool().clone();
//! let store   = SqliteEventStore::new(pool.clone());
//! let tasks   = TaskRepo::new(pool.clone());
//! let projects = ProjectRepo::new(pool);
//! # Ok(())
//! # }
//! ```

pub mod activity_repo;
pub mod agent_inbox_repo;
pub mod artifact_repo;
pub mod audit_finding_repo;
pub mod capability_profile_repo;
pub mod claim_repo;
pub mod comment_repo;
pub mod db;
pub mod device_repo;
pub mod document_repo;
mod entity_version;
pub mod event_store;
pub mod evidence_repo;
pub mod external_ref_repo;
pub mod handoff_repo;
pub mod idempotency_repo;
pub mod plan_repo;
pub mod project_repo;
pub mod project_settings_repo;
pub mod relation_repo;
pub mod repo_scope_repo;
pub mod rule_repo;
pub mod run_note_repo;
pub mod run_repo;
pub mod session_repo;
pub mod snapshot_repo;
pub mod task_complexity_repo;
pub mod task_repo;
pub mod tenant_quota_repo;
pub mod token_repo;
pub mod webhook_enrichment;
pub mod webhook_repo;
pub mod work_lease_repo;
pub mod work_unit_repo;
pub mod workspace_graph_repo;

pub use activity_repo::ActivityRepo;
pub use agent_inbox_repo::AgentInboxRepo;
pub use artifact_repo::ArtifactRepo;
pub use audit_finding_repo::{AuditFindingRepo, FindingFilter};
pub use capability_profile_repo::{CapabilityProfile, CapabilityProfileRepo};
pub use claim_repo::{ActiveClaim, AgentClaimRepo, ClaimOutcome};
pub use comment_repo::CommentRepo;
pub use db::Db;
pub use device_repo::{Device, DeviceRepo};
pub use document_repo::DocumentRepo;
pub use entity_version::{EntityVersion, EntityVersionRepo};
pub use event_store::SqliteEventStore;
pub use evidence_repo::EvidenceRepo;
pub use external_ref_repo::ExternalRefRepo;
pub use handoff_repo::HandoffRepo;
pub use idempotency_repo::IdempotencyRepo;
pub use plan_repo::PlanRepo;
pub use project_repo::ProjectRepo;
pub use project_settings_repo::ProjectSettingsRepo;
pub use relation_repo::RelationRepo;
pub use repo_scope_repo::RepoScopeRepo;
pub use rule_repo::RuleRepo;
pub use run_note_repo::RunNoteRepo;
pub use run_repo::RunRepo;
pub use session_repo::SessionRepo;
pub use snapshot_repo::{ProjectionSnapshot, Snapshot, SnapshotRepo};
pub use task_complexity_repo::TaskComplexityRepo;
pub use task_repo::{StuckTask, TaskRepo};
pub use tenant_quota_repo::TenantQuotaRepo;
pub use token_repo::TokenRepo;
pub use webhook_enrichment::WebhookEnrichment;
pub use webhook_repo::WebhookRepo;
pub use work_lease_repo::{ReserveOutcome, WorkLeaseRepo};
pub use work_unit_repo::WorkUnitRepo;
pub use workspace_graph_repo::{
    DuplicateTaskPair, GraphContextItem, GraphDirection, GraphEdge, GraphNeighborhood, GraphNode,
    GraphSearchHit, GraphStatus, WorkspaceGraphRepo,
};

/// RFC3339 → UTC timestamp for sqlite row mappers.
/// Shared helper — was copy-pasted into every repo module.
pub(crate) fn parse_ts(s: &str) -> daruma_shared::Result<daruma_shared::Timestamp> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| daruma_shared::CoreError::serde(e.to_string()))
}
