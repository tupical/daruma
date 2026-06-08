//! Shared application state — cheap to clone (all inner types are Arc or Clone).

use std::sync::Arc;

use taskagent_ai::OpenAiClient;
use taskagent_auth::TokenStore;
use taskagent_core::CommandBus;
use taskagent_events::EventStore;
use taskagent_storage::{
    ActivityRepo, AgentClaimRepo, AgentInboxRepo, CommentRepo, DocumentRepo, EntityVersionRepo,
    ExternalRefRepo, IdempotencyRepo, PlanRepo, ProjectRepo, RelationRepo, RunNoteRepo, RunRepo,
    SessionRepo, TaskComplexityRepo, TaskRepo, TenantQuotaRepo, TokenRepo, WebhookRepo,
    WorkLeaseRepo, WorkspaceGraphRepo,
};
use taskagent_sync::Hub;
use taskagent_webhooks::WebhookStore;

use crate::middleware::rate_limit::RateLimiter;

/// Application-wide state injected into every Axum handler via [`axum::extract::State`].
#[derive(Clone)]
pub struct AppState {
    /// Append-only event log (read-side for HTTP /events).
    pub store: Arc<dyn EventStore>,
    /// Task projection repo.
    pub tasks: Arc<TaskRepo>,
    /// Project projection repo.
    pub projects: Arc<ProjectRepo>,
    /// Comment projection repo.
    pub comments: Arc<CommentRepo>,
    /// Activity projection repo (denormalised user-facing history).
    pub activity: Arc<ActivityRepo>,
    /// Token storage (concrete) — used by the admin endpoints. The auth
    /// middleware uses the `TokenStore` trait object alongside this.
    pub tokens: Arc<TokenRepo>,
    /// Trait object handle for the auth middleware. Points at the same
    /// underlying repo as `tokens`.
    pub auth_store: Arc<dyn TokenStore>,
    /// Per-agent inbox cursors (used by `/v1/agents/{id}/inbox`).
    pub inbox: Arc<AgentInboxRepo>,
    /// Webhook configuration store (admin endpoints).
    pub webhooks: Arc<WebhookRepo>,
    /// Trait-object handle into the same webhook store, used by the
    /// dispatcher and reused by future webhook-aware paths.
    pub webhook_store: Arc<dyn WebhookStore>,
    /// Command dispatch entry-point.
    pub commands: CommandBus,
    /// WebSocket / event-bus bridge.
    pub hub: Arc<Hub>,
    /// Optional AI client — `None` when `OPENAI_API_KEY` is not set.
    pub ai: Option<OpenAiClient>,
    // ── Plan-domain repos (W3.1) ─────────────────────────────────────────────
    /// Plan projection repo.
    pub plans: Arc<PlanRepo>,
    /// Run projection repo.
    pub runs: Arc<RunRepo>,
    /// Run-note projection repo (§3.8.2).
    pub run_notes: Arc<RunNoteRepo>,
    /// Agent session repo.
    pub sessions: Arc<SessionRepo>,
    /// Optimistic task-claim repo.
    pub claims: Arc<AgentClaimRepo>,
    /// File/path work-lease repo (parallel-agent file coordination).
    pub work_leases: Arc<WorkLeaseRepo>,
    /// Cross-system identity mapping.
    pub external_refs: Arc<ExternalRefRepo>,
    /// Tenant quota checks for tasks/plans/storage.
    pub tenant_quotas: Arc<TenantQuotaRepo>,
    /// Idempotent command dedup (Linear A.1).
    pub idempotency: Arc<IdempotencyRepo>,
    // ── Relation-domain repo (§3.2 W3.1) ────────────────────────────────────
    /// Task-relation projection repo.
    pub relations: Arc<RelationRepo>,
    // ── Document-domain repo (PR1 §3-4) ──────────────────────────────────────
    /// Document projection repo.
    pub documents: Arc<DocumentRepo>,
    /// Immutable task/document version history repo.
    pub entity_versions: Arc<EntityVersionRepo>,
    // ── AI-derived projection (§3.8.3) ───────────────────────────────────────
    /// Per-task complexity hints produced by `taskagent_ai_analyze_complexity`.
    pub complexity_hints: Arc<TaskComplexityRepo>,
    /// WorkspaceGraph sidecar projection (derived read model).
    pub workspace_graph: Arc<WorkspaceGraphRepo>,
    /// Bundled `taskagent-mcp` binaries for authenticated download.
    pub mcp_downloads: crate::mcp_downloads::McpDownloads,
    /// In-memory per-workspace/per-token HTTP rate limiter.
    pub rate_limiter: RateLimiter,
}
