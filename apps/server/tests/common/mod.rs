//! Shared harness for the server-level integration tests.
//!
//! Each test file would otherwise repeat ~50 lines of `Db::memory` →
//! repo wiring → `AppState` construction. This module centralises that
//! into [`test_app`] and a small builder so tests can opt into the
//! handful of legitimate variations (bus capacity for lag-recovery,
//! a bound HTTP server for WS / `ApiClient` tests).
//!
//! `mod common;` is allowed inside each top-level `tests/*.rs`; Cargo
//! does not compile this file as a standalone test binary.

#![allow(dead_code)] // Different test files use different subsets.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use daruma_auth::{
    generate, Capabilities, NewTokenSpec, ProjectFilter, TokenKind, TokenScope, TokenStore,
};
use daruma_core::{CommandBus, CommandHandler, LifecycleGate};
use daruma_events::{EventBus, EventStore};
use daruma_server::{routes::router, state::AppState, workspace_graph};
use daruma_shared::AgentId;
use daruma_storage::{
    ActivityRepo, AgentClaimRepo, AgentInboxRepo, CommentRepo, Db, DocumentRepo, EntityVersionRepo,
    ExternalRefRepo, IdempotencyRepo, PlanRepo, ProjectRepo, RelationRepo, RunNoteRepo, RunRepo,
    SessionRepo, SqliteEventStore, TaskComplexityRepo, TaskRepo, TenantQuotaRepo, TokenRepo,
    WebhookRepo, WorkLeaseRepo, WorkspaceGraphRepo,
};
use daruma_sync::Hub;
use daruma_webhooks::WebhookStore;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;

const DEFAULT_BUS_CAPACITY: usize = 2048;
const DEFAULT_RATE_LIMIT: u32 = 300;

/// Fully wired in-memory server: router, state, event bus, and an
/// admin-scope token ready for `Authorization: Bearer …`.
pub struct TestApp {
    pub router: Router,
    pub state: AppState,
    pub bus: EventBus,
    pub admin_token: String,
    pub admin_agent_id: AgentId,
}

impl TestApp {
    pub fn auth_store(&self) -> Arc<dyn TokenStore> {
        self.state.auth_store.clone()
    }

    pub fn event_store(&self) -> Arc<dyn EventStore> {
        self.state.store.clone()
    }
}

pub struct TestAppBuilder {
    bus_capacity: usize,
    mint_admin: bool,
    admin_agent_id: Option<AgentId>,
    lifecycle_gate: Option<Arc<dyn LifecycleGate>>,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            bus_capacity: DEFAULT_BUS_CAPACITY,
            mint_admin: true,
            admin_agent_id: None,
            lifecycle_gate: None,
        }
    }
}

impl TestAppBuilder {
    pub fn bus_capacity(mut self, capacity: usize) -> Self {
        self.bus_capacity = capacity;
        self
    }

    /// Use a specific agent_id for the admin token (used by inbox tests
    /// that need to address the agent's cursor by id).
    pub fn admin_agent_id(mut self, id: AgentId) -> Self {
        self.admin_agent_id = Some(id);
        self
    }

    /// Wire a lifecycle gate into the command handler
    /// (docs/LIFECYCLE_RULES_SPEC.md §1.5).
    pub fn lifecycle_gate(mut self, gate: Arc<dyn LifecycleGate>) -> Self {
        self.lifecycle_gate = Some(gate);
        self
    }

    pub async fn build(self) -> TestApp {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();

        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let tokens = Arc::new(TokenRepo::new(pool.clone()));
        let devices = Arc::new(daruma_storage::DeviceRepo::new(pool.clone()));
        let inbox = Arc::new(AgentInboxRepo::new(pool.clone()));
        let webhooks = Arc::new(WebhookRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool.clone()));
        let plans = Arc::new(PlanRepo::new(pool.clone()));
        let runs = Arc::new(RunRepo::new(pool.clone()));
        let run_notes = Arc::new(RunNoteRepo::new(pool.clone()));
        let sessions = Arc::new(SessionRepo::new(pool.clone()));
        let claims = Arc::new(AgentClaimRepo::new(pool.clone()));
        let work_leases = Arc::new(WorkLeaseRepo::new(pool.clone()));
        let external_refs = Arc::new(ExternalRefRepo::new(pool.clone()));
        let documents = Arc::new(DocumentRepo::new(pool.clone()));
        let project_settings = Arc::new(daruma_storage::ProjectSettingsRepo::new(pool.clone()));
        let work_units = Arc::new(daruma_storage::WorkUnitRepo::new(pool.clone()));
        let handoffs = Arc::new(daruma_storage::HandoffRepo::new(pool.clone()));
        let capability_profiles =
            Arc::new(daruma_storage::CapabilityProfileRepo::new(pool.clone()));
        let rules = Arc::new(daruma_storage::RuleRepo::new(pool.clone()));
        let evidence = Arc::new(daruma_storage::EvidenceRepo::new(pool.clone()));
        let audit_findings = Arc::new(daruma_storage::AuditFindingRepo::new(pool.clone()));
        let complexity_hints = Arc::new(TaskComplexityRepo::new(pool.clone()));
        let idempotency = Arc::new(IdempotencyRepo::new(pool.clone()));
        let entity_versions = Arc::new(EntityVersionRepo::new(pool.clone()));
        let tenant_quotas = Arc::new(TenantQuotaRepo::new(pool.clone()));
        let relations = Arc::new(RelationRepo::new(pool));
        let auth_store: Arc<dyn TokenStore> = tokens.clone();
        let webhook_store: Arc<dyn WebhookStore> = webhooks.clone();

        let graph_db = Db::memory().await.unwrap();
        let workspace_graph = Arc::new(WorkspaceGraphRepo::new(graph_db.pool().clone()));
        workspace_graph.ensure_schema().await.unwrap();

        // `AgentId::new()` is a constructor that generates a fresh UUID v7 — not
        // a `Default` impl. The lint conflates the two.
        #[allow(clippy::unwrap_or_default)]
        let admin_agent_id = self.admin_agent_id.unwrap_or_else(AgentId::new);
        let admin_token = if self.mint_admin {
            let secret = generate(NewTokenSpec {
                kind: TokenKind::Svc,
                agent_id: admin_agent_id,
                scope: TokenScope::admin(),
                rate_limit_per_min: DEFAULT_RATE_LIMIT,
                expired_at: None,
            })
            .unwrap();
            auth_store.insert(secret.record.clone()).await.unwrap();
            secret.plaintext
        } else {
            String::new()
        };

        let bus = EventBus::new(self.bus_capacity);
        let mut handler = CommandHandler::new(
            store.clone(),
            tasks.clone(),
            projects.clone(),
            comments.clone(),
            activity.clone(),
            bus.clone(),
        )
        .with_plans(plans.clone())
        .with_runs(runs.clone())
        .with_run_notes(run_notes.clone())
        .with_sessions(sessions.clone())
        .with_claims(claims.clone())
        .with_work_leases(work_leases.clone())
        .with_external_refs(external_refs.clone())
        .with_tenant_quotas(tenant_quotas.clone())
        .with_documents(documents.clone())
        .with_project_settings(project_settings.clone())
        .with_work_units(work_units.clone())
        .with_handoffs(handoffs.clone())
        .with_capability_profiles(capability_profiles.clone())
        .with_rules(rules.clone())
        .with_evidence(evidence.clone())
        .with_relations(relations.clone());
        if let Some(gate) = self.lifecycle_gate.clone() {
            handler = handler.with_lifecycle_gate(gate);
        }
        let handler = Arc::new(handler);
        let command_bus = CommandBus::new(handler);
        let hub = Arc::new(Hub::new(bus.clone(), Arc::new(command_bus.clone())));

        workspace_graph::catch_up_from_events(&workspace_graph, &*store)
            .await
            .unwrap();
        workspace_graph::spawn_subscriber(workspace_graph.clone(), bus.clone());

        let state = AppState {
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
            commands: command_bus,
            hub,
            ai: None,
            plans,
            runs,
            run_notes,
            sessions,
            claims,
            work_leases,
            external_refs,
            idempotency,
            tenant_quotas,
            relations,
            documents,
            project_settings,
            work_units,
            handoffs,
            capability_profiles,
            rules,
            evidence,
            audit_findings,
            entity_versions,
            complexity_hints,
            workspace_graph,
            mcp_downloads: daruma_server::mcp_downloads::McpDownloads::default(),
            rate_limiter: daruma_server::middleware::rate_limit::RateLimiter::default(),
            pairing: daruma_discovery::PairingStore::new(),
            tls_host: "localhost:8443".to_string(),
            tls_fingerprint: "0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        };
        let router = router(state.clone());

        TestApp {
            router,
            state,
            bus,
            admin_token,
            admin_agent_id,
        }
    }
}

/// Shorthand for `TestAppBuilder::default().build()`.
pub async fn test_app() -> TestApp {
    TestAppBuilder::default().build().await
}

/// Bind a real `127.0.0.1` HTTP server and spawn it. Returns the bind
/// address; the caller keeps the [`TestApp`] for direct repo / bus access.
pub async fn spawn_server(app: &TestApp) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

// ── Token helpers ─────────────────────────────────────────────────────────────

/// Insert a freshly generated token of the given kind/scope into `store`.
/// Returns `(plaintext, agent_id)`.
pub async fn mint_token(
    store: &Arc<dyn TokenStore>,
    kind: TokenKind,
    scope: TokenScope,
) -> (String, AgentId) {
    let agent_id = AgentId::new();
    let secret = generate(NewTokenSpec {
        kind,
        agent_id,
        scope,
        rate_limit_per_min: DEFAULT_RATE_LIMIT,
        expired_at: None,
    })
    .unwrap();
    store.insert(secret.record.clone()).await.unwrap();
    (secret.plaintext, agent_id)
}

/// Mint a `Pat` token covering the full project filter with the given capabilities.
pub async fn mint_pat(
    store: &Arc<dyn TokenStore>,
    caps: Capabilities,
    projects: ProjectFilter,
) -> (String, AgentId) {
    mint_token(
        store,
        TokenKind::Pat,
        TokenScope {
            projects,
            capabilities: caps,
        },
    )
    .await
}

/// Mint a token of the given `kind` over `ProjectFilter::All` with the given
/// capabilities. Used by actor-propagation tests that toggle Bot vs Pat.
pub async fn mint_with_caps(
    store: &Arc<dyn TokenStore>,
    kind: TokenKind,
    caps: Capabilities,
) -> (String, AgentId) {
    mint_token(
        store,
        kind,
        TokenScope {
            projects: ProjectFilter::All,
            capabilities: caps,
        },
    )
    .await
}

// ── HTTP helpers (tower::ServiceExt::oneshot) ─────────────────────────────────

pub async fn json_get(app: Router, token: &str, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

pub async fn json_post(app: Router, token: &str, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}
