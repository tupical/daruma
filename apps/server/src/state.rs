//! Shared application state — cheap to clone (all inner types are Arc or Clone).

use std::sync::Arc;

use daruma_ai::OpenAiClient;
use daruma_auth::TokenStore;
use daruma_core::CommandBus;
use daruma_discovery::PairingStore;
use daruma_events::EventStore;
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, AgentInboxRepo, ArtifactRepo, AuditFindingRepo, CommentRepo,
    DeviceRepo, DocumentRepo, EntityVersionRepo, EvidenceRepo, ExternalRefRepo, IdempotencyRepo,
    PlanRepo, ProjectRepo, RelationRepo, RuleRepo, RunNoteRepo, RunRepo, SessionRepo,
    TaskComplexityRepo, TaskRepo, TenantQuotaRepo, TokenRepo, WebhookRepo, WorkLeaseRepo,
    WorkspaceGraphRepo,
};
use daruma_sync::Hub;
use daruma_webhooks::WebhookStore;
use sqlx::SqlitePool;

use crate::mcp_downloads::McpDownloads;
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
    /// Paired device identity/read model.
    pub devices: Arc<DeviceRepo>,
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
    /// Per-project settings projection (auto-append toggles).
    pub project_settings: Arc<daruma_storage::ProjectSettingsRepo>,
    /// Repo scope bindings (`scope_path → project_id`, migration 0046).
    /// Config table for MCP scope resolution — not event-sourced.
    pub repo_scopes: Arc<daruma_storage::RepoScopeRepo>,
    /// WorkUnit projection (P3 multi-agent coordination).
    pub work_units: Arc<daruma_storage::WorkUnitRepo>,
    pub handoffs: Arc<daruma_storage::HandoffRepo>,
    pub capability_profiles: Arc<daruma_storage::CapabilityProfileRepo>,
    /// Lifecycle-rule projection (docs/LIFECYCLE_RULES_SPEC.md §4).
    pub rules: Arc<RuleRepo>,
    /// Evidence-registry projection (OSS task 019eb65a-3185; spec §1.3).
    pub evidence: Arc<EvidenceRepo>,
    /// Artifact Registry projection (P4, migration 0036). Read-only HTTP
    /// surface; the projection is populated by `ArtifactRegistered`-family
    /// events. No command/write path is wired here.
    pub artifacts: Arc<ArtifactRepo>,
    /// Audit findings store (Audit primitives task B). Not event-sourced:
    /// written directly by the audit HTTP routes, read with severity/category/
    /// status filters. Feeds the Cloud Workspace Audit surface.
    pub audit_findings: Arc<AuditFindingRepo>,
    /// Immutable task/document version history repo.
    pub entity_versions: Arc<EntityVersionRepo>,
    // ── AI-derived projection (§3.8.3) ───────────────────────────────────────
    /// Per-task complexity hints produced by `daruma_ai_analyze_complexity`.
    pub complexity_hints: Arc<TaskComplexityRepo>,
    /// WorkspaceGraph sidecar projection (derived read model).
    pub workspace_graph: Arc<WorkspaceGraphRepo>,
    /// Bundled `daruma-mcp` binaries for authenticated download.
    pub mcp_downloads: crate::mcp_downloads::McpDownloads,
    /// In-memory per-workspace/per-token HTTP rate limiter.
    pub rate_limiter: RateLimiter,
    // ── LAN discovery + pairing (§3.3.5) ─────────────────────────────────────
    /// In-process single-use pairing token store (TTL 5 min).
    pub pairing: PairingStore,
    /// `host:port` string advertised in mDNS TXT and embedded in pairing URLs.
    pub tls_host: String,
    /// Hex SHA-256 fingerprint of the server's self-signed TLS certificate
    /// (without the `sha256:` prefix — callers prepend it as needed).
    pub tls_fingerprint: String,
    /// Lazily auto-provision a daruma project when an MCP call arrives under a
    /// repo `scope_path` that has no `repo_scopes` binding (title =
    /// basename(scope_path)). Default OFF in OSS/self-host; the mcpbox deploy
    /// turns it ON. Read from `DARUMA_AUTO_PROVISION_REPO_PROJECT`.
    pub auto_provision_repo_project: bool,
}

/// Truthy read of `DARUMA_AUTO_PROVISION_REPO_PROJECT` (default `false`).
pub fn env_auto_provision_repo_project() -> bool {
    std::env::var("DARUMA_AUTO_PROVISION_REPO_PROJECT")
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "on" | "yes"))
        .unwrap_or(false)
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        store: Arc<dyn EventStore>,
        tasks: Arc<TaskRepo>,
        projects: Arc<ProjectRepo>,
        comments: Arc<CommentRepo>,
        activity: Arc<ActivityRepo>,
        tokens: Arc<TokenRepo>,
        devices: Arc<DeviceRepo>,
        auth_store: Arc<dyn TokenStore>,
        inbox: Arc<AgentInboxRepo>,
        webhooks: Arc<WebhookRepo>,
        webhook_store: Arc<dyn WebhookStore>,
        commands: CommandBus,
        hub: Arc<Hub>,
        ai: Option<OpenAiClient>,
        plans: Arc<PlanRepo>,
        runs: Arc<RunRepo>,
        run_notes: Arc<RunNoteRepo>,
        sessions: Arc<SessionRepo>,
        claims: Arc<AgentClaimRepo>,
        work_leases: Arc<WorkLeaseRepo>,
        external_refs: Arc<ExternalRefRepo>,
        tenant_quotas: Arc<TenantQuotaRepo>,
        idempotency: Arc<IdempotencyRepo>,
        relations: Arc<RelationRepo>,
        documents: Arc<DocumentRepo>,
        entity_versions: Arc<EntityVersionRepo>,
        complexity_hints: Arc<TaskComplexityRepo>,
        project_settings: Arc<daruma_storage::ProjectSettingsRepo>,
        work_units: Arc<daruma_storage::WorkUnitRepo>,
        handoffs: Arc<daruma_storage::HandoffRepo>,
        capability_profiles: Arc<daruma_storage::CapabilityProfileRepo>,
        rules: Arc<RuleRepo>,
        workspace_graph: Arc<WorkspaceGraphRepo>,
        mcp_downloads: McpDownloads,
        pairing: PairingStore,
        tls_host: String,
        tls_fingerprint: String,
    ) -> Self {
        Self {
            store,
            tasks,
            projects,
            comments,
            activity,
            tokens,
            devices,
            auth_store,
            inbox,
            webhooks,
            webhook_store,
            commands,
            hub,
            ai,
            plans,
            runs,
            run_notes,
            sessions,
            claims,
            work_leases,
            external_refs,
            tenant_quotas,
            idempotency,
            relations,
            documents,
            project_settings,
            work_units,
            handoffs,
            capability_profiles,
            rules,
            evidence: Arc::new(EvidenceRepo::new(pool.clone())),
            artifacts: Arc::new(ArtifactRepo::new(pool.clone())),
            repo_scopes: Arc::new(daruma_storage::RepoScopeRepo::new(pool.clone())),
            audit_findings: Arc::new(AuditFindingRepo::new(pool)),
            entity_versions,
            complexity_hints,
            workspace_graph,
            mcp_downloads,
            rate_limiter: RateLimiter::default(),
            pairing,
            tls_host,
            tls_fingerprint,
            auto_provision_repo_project: env_auto_provision_repo_project(),
        }
    }
}
