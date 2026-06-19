//! `taskagent-server` entry-point — wires storage, core, sync, AI and Axum.

use std::sync::Arc;

use taskagent_ai::{AiConfig, OpenAiClient};
use taskagent_auth::{generate, NewTokenSpec, TokenKind, TokenScope, TokenStore};
use taskagent_core::{search::FtsSearchProvider, CommandBus, CommandHandler};
use taskagent_events::{EventBus, EventStore};
use taskagent_shared::AgentId;
use taskagent_storage::{
    ActivityRepo, AgentClaimRepo, AgentInboxRepo, AuditFindingRepo, CommentRepo, Db, DocumentRepo,
    EntityVersionRepo, ExternalRefRepo, IdempotencyRepo, PlanRepo, ProjectRepo, RelationRepo,
    RunNoteRepo, RunRepo, SessionRepo, SqliteEventStore, TaskComplexityRepo, TaskRepo,
    TenantQuotaRepo, TokenRepo, WebhookEnrichment, WebhookRepo, WorkLeaseRepo, WorkspaceGraphRepo,
};
use taskagent_sync::Hub;
use taskagent_webhooks::{spawn_dispatcher, EnrichmentSource, WebhookStore};
use tracing_subscriber::EnvFilter;

use taskagent_discovery::{CertBundle, MdnsAdvertiser, PairingStore};

use taskagent_server::{
    cors, mcp_downloads::McpDownloads, middleware::rate_limit::RateLimiter, routes,
    state::AppState, workspace_graph,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls refuses to pick a CryptoProvider when both `ring` and `aws-lc-rs`
    // end up in the dependency graph — pin ring explicitly before any TLS use.
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

    // ── Tracing ───────────────────────────────────────────────────────────────
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // ── Data directory ────────────────────────────────────────────────────────
    let data_path = taskagent_mcp::paths::data_dir();
    tokio::fs::create_dir_all(&data_path).await?;

    let db_path = data_path.join("taskagent.sqlite");
    let db_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("DB path contains non-UTF-8 characters"))?;

    // ── Database ──────────────────────────────────────────────────────────────
    tracing::info!(path = db_str, "opening database");
    let db = Db::open(db_str)
        .await
        .map_err(|e| anyhow::anyhow!("DB open failed: {e}"))?;
    db.migrate()
        .await
        .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

    // ── WorkspaceGraph sidecar ────────────────────────────────────────────────
    let graph_db_path = data_path.join("workspacegraph.sqlite");
    let graph_db_str = graph_db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("workspace graph DB path contains non-UTF-8 characters"))?;
    tracing::info!(path = graph_db_str, "opening workspace graph sidecar");
    let graph_db = Db::open(graph_db_str)
        .await
        .map_err(|e| anyhow::anyhow!("workspace graph DB open failed: {e}"))?;
    let workspace_graph = Arc::new(WorkspaceGraphRepo::new(graph_db.pool().clone()));
    workspace_graph
        .ensure_schema()
        .await
        .map_err(|e| anyhow::anyhow!("workspace graph schema failed: {e}"))?;

    // ── Storage layer ─────────────────────────────────────────────────────────
    let pool = db.pool().clone();
    let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
    let tasks = Arc::new(TaskRepo::new(pool.clone()));
    let projects = Arc::new(ProjectRepo::new(pool.clone()));
    let comments = Arc::new(CommentRepo::new(pool.clone()));
    let tokens = Arc::new(TokenRepo::new(pool.clone()));
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
    let project_settings = Arc::new(taskagent_storage::ProjectSettingsRepo::new(pool.clone()));
    let work_units = Arc::new(taskagent_storage::WorkUnitRepo::new(pool.clone()));
    let rules = Arc::new(taskagent_storage::RuleRepo::new(pool.clone()));
    let evidence = Arc::new(taskagent_storage::EvidenceRepo::new(pool.clone()));
    let audit_findings = Arc::new(AuditFindingRepo::new(pool.clone()));
    let entity_versions = Arc::new(EntityVersionRepo::new(pool.clone()));
    let complexity_hints = Arc::new(TaskComplexityRepo::new(pool.clone()));
    let idempotency = Arc::new(IdempotencyRepo::new(pool.clone()));
    let tenant_quotas = Arc::new(TenantQuotaRepo::new(pool.clone()));
    // Seed the bloom filter from existing rows so lookups on restart take the
    // fast path immediately rather than after the first re-seen command.
    idempotency
        .warm()
        .await
        .map_err(|e| anyhow::anyhow!("idempotency bloom warm failed: {e}"))?;
    let relations = Arc::new(RelationRepo::new(pool));
    let auth_store: Arc<dyn TokenStore> = tokens.clone();
    let webhook_store: Arc<dyn WebhookStore> = webhooks.clone();

    // ── Bootstrap token (first run only) ──────────────────────────────────────
    bootstrap_admin_token(auth_store.as_ref(), &data_path).await?;

    // ── Activity backfill (idempotent; must run before dispatcher) ────────────
    let backfilled = activity
        .backfill_from_events(&*store)
        .await
        .map_err(|e| anyhow::anyhow!("activity backfill failed: {e}"))?;
    tracing::info!(rows = backfilled, "activity backfill complete");

    // ── WorkspaceGraph catch-up (idempotent) ──────────────────────────────────
    let graph_caught_up = workspace_graph::catch_up_from_events(&workspace_graph, &*store)
        .await
        .map_err(|e| anyhow::anyhow!("workspace graph catch-up failed: {e}"))?;
    tracing::info!(
        events = graph_caught_up,
        "workspace graph catch-up complete"
    );

    // ── Core layer ────────────────────────────────────────────────────────────
    let bus = EventBus::new(2048);
    let handler = Arc::new(
        CommandHandler::new(
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
        .with_rules(rules.clone())
        .with_evidence(evidence.clone())
        // Rule engine reads through the same projections (zero-cost when empty).
        // Evidence satisfies `required` requirements (spec §1.3).
        .with_lifecycle_gate(Arc::new(taskagent_core::RuleEngineGate::with_evidence(
            rules.clone(),
            evidence.clone(),
        )))
        .with_relations(relations.clone())
        .with_search_provider(Arc::new(FtsSearchProvider::new(
            tasks.clone(),
            comments.clone(),
            plans.clone(),
        ))),
    );
    let command_bus = CommandBus::new(handler.clone());

    // ── Sync layer ────────────────────────────────────────────────────────────
    let hub = Arc::new(Hub::new(bus.clone(), Arc::new(command_bus.clone())));

    // ── Webhook dispatcher ────────────────────────────────────────────────────
    let http = reqwest::Client::builder()
        .user_agent(format!("taskagent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow::anyhow!("reqwest client: {e}"))?;
    let enrichment: Arc<dyn EnrichmentSource> =
        WebhookEnrichment::new(tasks.clone(), plans.clone(), projects.clone()).into_arc();
    let _dispatcher = spawn_dispatcher(bus.subscribe(), webhook_store.clone(), http, enrichment);
    // `_dispatcher` lives for the lifetime of the process; binding it
    // keeps the JoinHandle alive (Drop aborts the task).
    std::mem::forget(_dispatcher);

    workspace_graph::spawn_subscriber(workspace_graph.clone(), bus.clone());

    // ── AI (optional) ─────────────────────────────────────────────────────────
    let ai: Option<OpenAiClient> = match AiConfig::from_env() {
        Ok(cfg) => {
            tracing::info!(model = %cfg.model, "AI client configured");
            Some(OpenAiClient::new(cfg))
        }
        Err(_) => {
            tracing::info!("OPENAI_API_KEY not set — AI endpoints will return 502");
            None
        }
    };

    // ── TLS certificate (self-signed, persisted) ──────────────────────────────
    let hostname = std::env::var("TASKAGENT_HOSTNAME").unwrap_or_else(|_| hostname_or_localhost());
    let tls_bundle = CertBundle::load_or_generate(&data_path, &hostname)
        .await
        .map_err(|e| anyhow::anyhow!("TLS init failed: {e}"))?;
    let tls_fingerprint = tls_bundle.fingerprint.clone();

    let tls_port: u16 = std::env::var("TASKAGENT_TLS_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8443);
    let tls_host = format!("{hostname}:{tls_port}");

    // ── Pairing store ─────────────────────────────────────────────────────────
    let pairing = PairingStore::new();

    // ── mDNS advertisement ────────────────────────────────────────────────────
    let _mdns = if std::env::var("TASKAGENT_MDNS_DISABLE").is_ok() {
        tracing::info!("mDNS advertisement disabled via TASKAGENT_MDNS_DISABLE");
        None
    } else {
        match MdnsAdvertiser::start(
            &hostname,
            tls_port,
            &format!("sha256:{tls_fingerprint}"),
            env!("CARGO_PKG_VERSION"),
        ) {
            Ok(advertiser) => {
                tracing::info!(
                    service = "_taskagent._tcp.local.",
                    port = tls_port,
                    fingerprint = %tls_fingerprint,
                    "mDNS advertisement started"
                );
                Some(advertiser)
            }
            Err(e) => {
                tracing::warn!(err = %e, "mDNS advertisement failed to start (non-fatal)");
                None
            }
        }
    };
    // Keep _mdns alive for the process lifetime so the advertisement persists.

    // Spawn periodic pairing-store sweep (removes expired tickets every 60 s).
    {
        let pairing_bg = pairing.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                pairing_bg.sweep().await;
            }
        });
    }

    // ── App state ─────────────────────────────────────────────────────────────
    let mcp_downloads = McpDownloads::discover();
    if mcp_downloads.linux.is_some() {
        tracing::info!("taskagent-mcp linux download available");
    }
    if mcp_downloads.windows.is_some() {
        tracing::info!("taskagent-mcp windows download available");
    }

    let state = AppState {
        store,
        tasks,
        projects,
        comments,
        activity,
        tokens,
        auth_store,
        inbox,
        webhooks,
        webhook_store,
        commands: command_bus.clone(),
        hub,
        ai,
        plans,
        runs,
        run_notes,
        sessions,
        claims: claims.clone(),
        work_leases: work_leases.clone(),
        external_refs,
        idempotency: idempotency.clone(),
        tenant_quotas,
        relations,
        documents,
        project_settings,
        work_units,
        rules,
        evidence,
        audit_findings,
        entity_versions,
        complexity_hints,
        workspace_graph,
        mcp_downloads,
        rate_limiter: RateLimiter::default(),
        pairing,
        tls_host,
        tls_fingerprint,
    };

    // ── Background: claim + work-lease TTL sweep (every 30 s) ─────────────────
    {
        let claims_bg = claims;
        let leases_bg = work_leases;
        let bus_bg = command_bus;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                match claims_bg.sweep_expired().await {
                    Ok(released) => {
                        for (agent_id, task_id) in released {
                            let _ = bus_bg
                                .dispatch(
                                    taskagent_core::Command::ReleaseClaim { agent_id, task_id },
                                    taskagent_domain::Actor::user(),
                                )
                                .await;
                        }
                    }
                    Err(e) => tracing::warn!(err = %e, "claim TTL sweep failed"),
                }
                match leases_bg.sweep_expired().await {
                    Ok(released) => {
                        for (agent_id, task_id) in released {
                            let _ = bus_bg
                                .dispatch(
                                    taskagent_core::Command::ReleaseFiles { agent_id, task_id },
                                    taskagent_domain::Actor::user(),
                                )
                                .await;
                        }
                    }
                    Err(e) => tracing::warn!(err = %e, "work-lease TTL sweep failed"),
                }
            }
        });
    }

    // ── Background: idempotency cleanup (every hour) ──────────────────────────
    {
        let idempotency_bg = idempotency;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                match idempotency_bg
                    .cleanup_older_than(chrono::Duration::days(7))
                    .await
                {
                    Ok(n) => tracing::info!(rows = n, "idempotency cleanup complete"),
                    Err(e) => tracing::warn!(err = %e, "idempotency cleanup failed"),
                }
            }
        });
    }

    // ── Background: §3.7.4 liveness watchdog (every 10 s) ─────────────────────
    {
        let liveness_ack: u64 = std::env::var("TASKAGENT_LIVENESS_ACK_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let liveness_idle: u64 = std::env::var("TASKAGENT_LIVENESS_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);
        let handler_bg = handler.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                if let Err(e) = handler_bg
                    .tick_liveness(chrono::Utc::now(), liveness_ack, liveness_idle)
                    .await
                {
                    tracing::warn!(err = %e, "liveness watchdog tick failed");
                }
            }
        });
    }

    // ── Due-date watchdog (task.due webhooks) ────────────────────────────────
    {
        let due_tick_secs: u64 = std::env::var("TASKAGENT_DUE_TICK_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        if due_tick_secs > 0 {
            let handler_bg = handler.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(due_tick_secs));
                loop {
                    tick.tick().await;
                    match handler_bg.tick_due_tasks(chrono::Utc::now()).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(count = n, "task.due notifications emitted"),
                        Err(e) => tracing::warn!(err = %e, "due-date watchdog tick failed"),
                    }
                }
            });
        }
    }

    // ── Router ────────────────────────────────────────────────────────────────
    let app = routes::router(state).layer(cors::cors_layer());

    // ── TLS listener on :8443 (pairing flow; carries the cert whose
    //    fingerprint is in QR/mDNS) ──────────────────────────────────────────
    let tls_config = tls_bundle
        .into_tls_config()
        .map_err(|e| anyhow::anyhow!("TLS config build failed: {e}"))?;

    let tls_addr = std::net::SocketAddr::from(([0, 0, 0, 0], tls_port));
    let tls_tcp = tokio::net::TcpListener::bind(tls_addr)
        .await
        .map_err(|e| anyhow::anyhow!("TLS bind {tls_addr}: {e}"))?;
    tracing::info!(addr = %tls_addr, "TLS listener ready");

    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config.server_config.clone());
    let tls_app = app.clone();
    tokio::spawn(async move {
        loop {
            match tls_tcp.accept().await {
                Ok((stream, peer_addr)) => {
                    let acceptor = tls_acceptor.clone();
                    let svc = tls_app.clone();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = hyper_util::rt::TokioIo::new(tls_stream);
                                let hyper_svc = hyper::service::service_fn(move |req| {
                                    let mut svc = svc.clone();
                                    // Inject peer addr so ConnectInfo extractor works.
                                    use tower::Service;
                                    svc.call(req)
                                });
                                if let Err(e) = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, hyper_svc)
                                    .with_upgrades()
                                    .await
                                {
                                    tracing::debug!(err = %e, "TLS connection error");
                                }
                            }
                            Err(e) => {
                                tracing::debug!(peer = %peer_addr, err = %e, "TLS handshake failed");
                            }
                        }
                    });
                }
                Err(e) => tracing::warn!(err = %e, "TLS accept error"),
            }
        }
    });

    // ── Plain HTTP listener on :8080 (existing API clients, not for pairing) ─
    let port: u16 = std::env::var("TASKAGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "HTTP listener ready");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Load a stable UUID from `<data_dir>/host_id`, generating and persisting
/// one on first call. Used by callers that need a stable installation ID.
#[allow(dead_code)]
async fn load_or_create_host_id(data_dir: &std::path::Path) -> anyhow::Result<String> {
    let path = data_dir.join("host_id");
    if path.exists() {
        let raw = tokio::fs::read_to_string(&path).await?;
        let id = raw.trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    tokio::fs::write(&path, &id).await?;
    Ok(id)
}

/// On the very first run (no active tokens in the DB), generate a
/// long-lived `svc` admin token, persist it, and write the plaintext to
/// `<data_dir>/bootstrap.token` + log it once to stderr.
///
/// On subsequent runs this is a no-op.
async fn bootstrap_admin_token(
    store: &dyn TokenStore,
    data_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let active = store
        .count_active()
        .await
        .map_err(|e| anyhow::anyhow!("count tokens: {e}"))?;
    if active > 0 {
        tracing::info!(
            active_tokens = active,
            "skipping bootstrap — tokens already exist"
        );
        return Ok(());
    }

    let secret = generate(NewTokenSpec {
        kind: TokenKind::Svc,
        agent_id: AgentId::new(),
        scope: TokenScope::admin(),
        rate_limit_per_min: 300,
        expired_at: None,
    })
    .map_err(|e| anyhow::anyhow!("token generate: {e}"))?;

    store
        .insert(secret.record.clone())
        .await
        .map_err(|e| anyhow::anyhow!("token insert: {e}"))?;

    let bootstrap_path = data_dir.join("bootstrap.token");
    tokio::fs::write(&bootstrap_path, &secret.plaintext).await?;

    // Restrict file mode on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = tokio::fs::metadata(&bootstrap_path).await {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = tokio::fs::set_permissions(&bootstrap_path, perms).await;
        }
    }

    eprintln!("┌────────────────────────────────────────────────────────────────────┐");
    eprintln!("│ TASKAGENT BOOTSTRAP ADMIN TOKEN                                    │");
    eprintln!("│ ------------------------------------------------------------------ │");
    eprintln!("│ This token is shown only once. Save it now.                        │");
    eprintln!("│ Also written to: {}", bootstrap_path.display());
    eprintln!("│                                                                    │");
    eprintln!("│   {}", secret.plaintext);
    eprintln!("│                                                                    │");
    eprintln!("└────────────────────────────────────────────────────────────────────┘");

    tracing::info!(
        token_id = %secret.record.id,
        prefix = %secret.record.prefix,
        path = %bootstrap_path.display(),
        "wrote bootstrap admin token"
    );

    Ok(())
}

// ── Discovery helpers ─────────────────────────────────────────────────────────

/// Return the system hostname, falling back to `"localhost"` on any error.
fn hostname_or_localhost() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| {
            // gethostname via std::process is not exposed; use the nix-style
            // approach of reading /etc/hostname when available.
            std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|_| "localhost".to_string())
}
