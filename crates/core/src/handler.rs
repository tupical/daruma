//! Command handler — validates commands, emits events, updates projections,
//! publishes to the event bus.

use std::sync::Arc;

use daruma_domain::{
    Actor, ActorRef, AgentSession, Comment, DocumentKind, PlanPatch, PlanStatus, Project, Relation,
    Run, RunOutcome, RunStatus, SessionArtifact, SignalKind, Status, Task,
};
use daruma_events::{event::ObsolescenceKind, Event, EventBus, EventEnvelope, EventStore};
use daruma_shared::{
    time, AgentId, AgentSessionId, CoreError, DocumentId, PlanId, ProjectId, RelationId, Result,
    RunId, RunNoteId, SessionArtifactId, TaskId,
};
use daruma_storage::{
    ActivityRepo, CommentRepo, ProjectRepo, ProjectSettingsRepo, RelationRepo, TaskRepo,
    TenantQuotaRepo, WorkUnitRepo,
};

use crate::{
    plan_concurrency::detect_parent_cycle,
    relation_enforcement,
    repos::{
        AgentClaimRepository, DocumentRepository, EvidenceRepository, ExternalRefRepository,
        PlanRepository, RuleRepository, RunNoteRepository, RunRepository, SessionRepository,
        WorkLeaseRepository,
    },
    search::{index_items_for_event, SearchProvider},
    Command,
};

use crate::lifecycle_gate::{
    derive_gate_checks, gate_override_of, DispatchOutcome, GateCheck, GateDecision, LifecycleGate,
};
use daruma_events::event::RuleDecision as EventRuleDecision;

/// Processes commands: validate → build events → persist → apply → publish.
pub struct CommandHandler {
    pub store: Arc<dyn EventStore>,
    pub tasks: Arc<TaskRepo>,
    pub projects: Arc<ProjectRepo>,
    pub comments: Arc<CommentRepo>,
    pub activity: Arc<ActivityRepo>,
    pub bus: EventBus,

    // Relation repo — None until W2.1 is wired; LinkTasks/UnlinkTasks return
    // CoreError::Storage when not configured.
    pub relations: Option<Arc<RelationRepo>>,

    // Plan-domain repos — None until W2.1 is wired; commands that require them
    // return CoreError::Storage when not configured.
    pub plans: Option<Arc<dyn PlanRepository>>,
    pub runs: Option<Arc<dyn RunRepository>>,
    pub run_notes: Option<Arc<dyn RunNoteRepository>>,
    pub sessions: Option<Arc<dyn SessionRepository>>,
    pub claims: Option<Arc<dyn AgentClaimRepository>>,
    pub work_leases: Option<Arc<dyn WorkLeaseRepository>>,
    /// Per-project settings projection (auto-append toggles). `None` only in
    /// minimal test harnesses; when absent, defaults apply and the settings
    /// command is rejected.
    pub project_settings: Option<Arc<ProjectSettingsRepo>>,
    /// WorkUnit projection (P3). `None` until wired by the server.
    pub work_units: Option<Arc<WorkUnitRepo>>,
    pub handoffs: Option<Arc<daruma_storage::HandoffRepo>>,
    pub capability_profiles: Option<Arc<daruma_storage::CapabilityProfileRepo>>,
    pub external_refs: Option<Arc<dyn ExternalRefRepository>>,
    pub tenant_quotas: Option<Arc<TenantQuotaRepo>>,

    // Document-domain repo (PR1 §3-4) — None until wired; CreateDocument and
    // related commands will return CoreError::Storage when not configured.
    pub documents: Option<Arc<dyn DocumentRepository>>,

    // Optional async search indexing pipeline. Failures are logged and do not
    // make command dispatch fail.
    pub search_provider: Option<Arc<dyn SearchProvider>>,

    // Lifecycle gate (docs/LIFECYCLE_RULES_SPEC.md §1.5) — pre-persist
    // allowed/warning/blocked checks on derived trigger points. `None`
    // (the default) is zero-cost: no derivation, no lookups.
    pub lifecycle_gate: Option<Arc<dyn LifecycleGate>>,

    // Lifecycle-rule projection (docs/LIFECYCLE_RULES_SPEC.md §4). `None`
    // until wired; the rule CRUD commands return CoreError::Storage when
    // absent. The gate above reads through this same repo.
    pub rules: Option<Arc<dyn RuleRepository>>,

    // Evidence-registry projection (OSS task 019eb65a-3185; spec §1.3). `None`
    // until wired; `RecordEvidence` returns CoreError::Storage when absent. The
    // gate reads through this repo to satisfy `required` requirements.
    pub evidence: Option<Arc<dyn EvidenceRepository>>,
}

impl CommandHandler {
    /// Construct a handler with the core task/project/comment repos.
    /// Plan-domain repos default to `None`; use the builder methods below.
    pub fn new(
        store: Arc<dyn EventStore>,
        tasks: Arc<TaskRepo>,
        projects: Arc<ProjectRepo>,
        comments: Arc<CommentRepo>,
        activity: Arc<ActivityRepo>,
        bus: EventBus,
    ) -> Self {
        Self {
            store,
            tasks,
            projects,
            comments,
            activity,
            bus,
            relations: None,
            plans: None,
            runs: None,
            run_notes: None,
            sessions: None,
            claims: None,
            work_leases: None,
            external_refs: None,
            tenant_quotas: None,
            documents: None,
            project_settings: None,
            work_units: None,
            handoffs: None,
            capability_profiles: None,
            search_provider: None,
            lifecycle_gate: None,
            rules: None,
            evidence: None,
        }
    }

    /// Wire a `RelationRepo` implementation (§3.2 W2.1).
    pub fn with_relations(mut self, repo: Arc<RelationRepo>) -> Self {
        self.relations = Some(repo);
        self
    }

    /// Wire a `PlanRepository` implementation (called after W2.1 lands).
    pub fn with_plans(mut self, repo: Arc<dyn PlanRepository>) -> Self {
        self.plans = Some(repo);
        self
    }

    /// Wire a `RunRepository` implementation.
    pub fn with_runs(mut self, repo: Arc<dyn RunRepository>) -> Self {
        self.runs = Some(repo);
        self
    }

    /// Wire a `RunNoteRepository` implementation (§3.8.2).
    pub fn with_run_notes(mut self, repo: Arc<dyn RunNoteRepository>) -> Self {
        self.run_notes = Some(repo);
        self
    }

    /// Wire a `SessionRepository` implementation.
    pub fn with_sessions(mut self, repo: Arc<dyn SessionRepository>) -> Self {
        self.sessions = Some(repo);
        self
    }

    /// Wire an `AgentClaimRepository` implementation.
    pub fn with_claims(mut self, repo: Arc<dyn AgentClaimRepository>) -> Self {
        self.claims = Some(repo);
        self
    }

    /// Wire a `WorkLeaseRepository` implementation.
    pub fn with_work_leases(mut self, repo: Arc<dyn WorkLeaseRepository>) -> Self {
        self.work_leases = Some(repo);
        self
    }

    /// Wire the per-project settings projection (auto-append toggles).
    pub fn with_project_settings(mut self, repo: Arc<ProjectSettingsRepo>) -> Self {
        self.project_settings = Some(repo);
        self
    }

    /// Wire the WorkUnit projection (P3).
    pub fn with_handoffs(mut self, repo: Arc<daruma_storage::HandoffRepo>) -> Self {
        self.handoffs = Some(repo);
        self
    }

    pub fn with_capability_profiles(
        mut self,
        repo: Arc<daruma_storage::CapabilityProfileRepo>,
    ) -> Self {
        self.capability_profiles = Some(repo);
        self
    }

    pub fn with_work_units(mut self, repo: Arc<WorkUnitRepo>) -> Self {
        self.work_units = Some(repo);
        self
    }

    /// Wire an `ExternalRefRepository` implementation.
    pub fn with_external_refs(mut self, repo: Arc<dyn ExternalRefRepository>) -> Self {
        self.external_refs = Some(repo);
        self
    }

    pub fn with_tenant_quotas(mut self, repo: Arc<TenantQuotaRepo>) -> Self {
        self.tenant_quotas = Some(repo);
        self
    }

    /// Wire a `DocumentRepository` implementation (PR1 §3-4).
    pub fn with_documents(mut self, repo: Arc<dyn DocumentRepository>) -> Self {
        self.documents = Some(repo);
        self
    }

    /// Wire an optional search indexer. Indexing is asynchronous and best-effort.
    pub fn with_search_provider(mut self, provider: Arc<dyn SearchProvider>) -> Self {
        self.search_provider = Some(provider);
        self
    }

    /// Wire the lifecycle-rule projection (CRUD + gate reads).
    pub fn with_rules(mut self, repo: Arc<dyn RuleRepository>) -> Self {
        self.rules = Some(repo);
        self
    }

    /// Wire the evidence-registry projection (`RecordEvidence` + gate reads).
    pub fn with_evidence(mut self, repo: Arc<dyn EvidenceRepository>) -> Self {
        self.evidence = Some(repo);
        self
    }

    /// Wire a lifecycle gate (rules engine). See docs/LIFECYCLE_RULES_SPEC.md.
    pub fn with_lifecycle_gate(mut self, gate: Arc<dyn LifecycleGate>) -> Self {
        self.lifecycle_gate = Some(gate);
        self
    }

    /// Validate the command, persist resulting events, update projections, and
    /// broadcast via the event bus. Returns the persisted envelopes (with seq
    /// assigned).  Returns an empty Vec for no-op commands (e.g. SetStatus
    /// when the status is already the requested value).
    pub async fn handle(&self, cmd: Command, actor: Actor) -> Result<Vec<EventEnvelope>> {
        self.handle_with_warnings(cmd, actor)
            .await
            .map(|outcome| outcome.events)
    }

    /// Like [`Self::handle`], but also returns lifecycle-gate warnings so
    /// transports can surface them in `MutationResponse.warnings`
    /// (docs/LIFECYCLE_RULES_SPEC.md §1.5). Blocked checks abort BEFORE
    /// persist with `CoreError::Conflict("rule_blocked: …")`; trigger points
    /// are derived from the built (not yet persisted) events, so every path
    /// to a transition — `SetStatus`, `CompleteTask`, bulk, drain — is
    /// covered by this single call site (spec §3, invariant 7).
    pub async fn handle_with_warnings(
        &self,
        cmd: Command,
        actor: Actor,
    ) -> Result<DispatchOutcome> {
        let gate_override = self.lifecycle_gate.as_ref().map(|_| gate_override_of(&cmd));
        let events = self.build_events(cmd, &actor).await?;
        if events.is_empty() {
            return Ok(DispatchOutcome {
                events: vec![],
                warnings: vec![],
            });
        }

        let mut warnings = Vec::new();
        // Rule-engine audit trail: a `RuleFired` event per acting rule. Only
        // warnings/blocks act, so `Allowed` decisions add nothing — an
        // unconstrained workspace stays silent (spec §1.5; task risk note).
        let mut rule_audit: Vec<Event> = Vec::new();
        if let Some(gate) = &self.lifecycle_gate {
            let gate_override = gate_override.unwrap_or_default();
            for check in derive_gate_checks(&events) {
                match gate.check(&actor, &check, &gate_override).await? {
                    GateDecision::Allowed => {}
                    GateDecision::Warning(mut batch) => {
                        rule_audit.extend(rule_fired_events(
                            &check,
                            &actor,
                            EventRuleDecision::Warning,
                            batch.iter().map(|w| (&w.details, w.message.as_str())),
                        ));
                        warnings.append(&mut batch);
                    }
                    GateDecision::Blocked { message, details } => {
                        // Persist the block's audit trail before aborting, so a
                        // rejected transition is still visible in the event log
                        // / webhooks even though the mutation never lands.
                        let blocked = blocked_outcomes(&details, &message);
                        let audit = rule_fired_events(
                            &check,
                            &actor,
                            EventRuleDecision::Blocked,
                            blocked.iter().map(|(d, m)| (d, m.as_str())),
                        );
                        if !audit.is_empty() {
                            let envs = audit
                                .into_iter()
                                .map(|payload| EventEnvelope::new(actor.clone(), payload))
                                .collect();
                            let persisted = self.store.append_batch(envs).await?;
                            for env in &persisted {
                                self.activity.apply_event(env).await?;
                                self.spawn_search_index(env.clone());
                                self.bus.publish(env.clone());
                            }
                        }
                        return Err(CoreError::conflict(format!("rule_blocked: {message}")));
                    }
                }
            }
        }

        // Audit events ride ahead of the mutation they describe (same actor),
        // so a subscriber sees "rule warned" then the transition it warned on.
        let envelopes: Vec<EventEnvelope> = rule_audit
            .into_iter()
            .chain(events)
            .map(|payload| EventEnvelope::new(actor.clone(), payload))
            .collect();

        let persisted = self.store.append_batch(envelopes).await?;

        for env in &persisted {
            self.tasks.apply_event(env).await?;
            self.projects.apply_event(env).await?;
            self.comments.apply_event(env).await?;
            self.activity.apply_event(env).await?;
            if let Some(relations) = &self.relations {
                relations.apply_event(&env.payload).await;
            }
            if let Some(plans) = &self.plans {
                plans.apply_event(env).await?;
            }
            if let Some(runs) = &self.runs {
                runs.apply_event(env).await?;
            }
            if let Some(run_notes) = &self.run_notes {
                run_notes.apply_event(env).await?;
            }
            if let Some(sessions) = &self.sessions {
                sessions.apply_event(env).await?;
            }
            if let Some(claims) = &self.claims {
                claims.apply_event(env).await?;
            }
            if let Some(work_leases) = &self.work_leases {
                work_leases.apply_event(env).await?;
            }
            if let Some(ext) = &self.external_refs {
                ext.apply_event(env).await?;
            }
            if let Some(documents) = &self.documents {
                documents.apply_event(env).await?;
            }
            if let Some(settings) = &self.project_settings {
                settings.apply_event(env).await?;
            }
            // Capability profiles read the unit's owner, which the work-unit
            // projector clears on completion/release — so profiles fold their
            // signal first (P6).
            if let Some(profiles) = &self.capability_profiles {
                profiles.apply_event(env).await?;
            }
            if let Some(work_units) = &self.work_units {
                work_units.apply_event(env).await?;
            }
            if let Some(handoffs) = &self.handoffs {
                handoffs.apply_event(env).await?;
            }
            if let Some(rules) = &self.rules {
                rules.apply_event(env).await?;
            }
            if let Some(evidence) = &self.evidence {
                evidence.apply_event(env).await?;
            }
            self.spawn_search_index(env.clone());
            self.bus.publish(env.clone());
        }

        // Best-effort auto-append into the project's Interview / Human Log
        // documents (toggleable per project, ON by default). Never fails the
        // command; emits its own DocumentContentAppended events.
        self.auto_append_logs(&persisted).await;

        Ok(DispatchOutcome {
            events: persisted,
            warnings,
        })
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Build the events for transitioning a single task to `to`, including
    /// the side-effects: blocker rejection, `TaskReopened`/`TaskClosed`,
    /// downstream `TaskUnblocked` cascade, and `Blocks → WasBlocking`
    /// migration on outgoing edges.
    ///
    /// Returns an empty vec when `task.status == to` (no-op). Used by both
    /// `Command::SetStatus` and `Command::BulkSetStatus` so the two paths
    /// stay in lockstep.
    async fn emit_status_transition_events(
        &self,
        task: &Task,
        to: Status,
        actor: &Actor,
        now: daruma_shared::Timestamp,
    ) -> Result<Vec<Event>> {
        let id = task.id;
        if task.status == to {
            return Ok(vec![]);
        }
        let from = task.status;

        // Blocker check: when transitioning to Done, reject if any active
        // (non-Done) blocker exists.
        if to == Status::Done {
            if let Some(relations) = &self.relations {
                let blocked_by =
                    relation_enforcement::list_active_blockers(relations, &self.tasks, id).await?;
                if !blocked_by.is_empty() {
                    return Err(CoreError::conflict(format!(
                        "task_blocked: task {id} blocked by {} task(s): {:?}",
                        blocked_by.len(),
                        blocked_by
                    )));
                }
            }
        }

        let mut events = vec![Event::TaskStatusChanged {
            task_id: id,
            from,
            to,
        }];
        if from.is_terminal() && !to.is_terminal() {
            events.push(Event::TaskReopened {
                task_id: id,
                by: actor.clone(),
                at: now,
            });
        } else if !from.is_terminal() && to.is_terminal() {
            events.push(Event::TaskClosed {
                task_id: id,
                by: actor.clone(),
                at: now,
            });
        }

        // Downstream cascade + Blocks→WasBlocking migration when reaching Done.
        if to == Status::Done {
            if let Some(relations) = &self.relations {
                let to_unblock =
                    relation_enforcement::list_downstream_to_unblock(relations, &self.tasks, id)
                        .await?;
                for task_id in to_unblock {
                    events.push(Event::TaskUnblocked {
                        task_id,
                        unblocked_by: id,
                        occurred_at: now,
                    });
                }

                // §3.7.2 / LIN A.3: convert every active `Blocks` edge outgoing
                // from `id` to `WasBlocking` for audit.
                let outgoing = relations.list_blocks_targets(id).await?;
                for rel in outgoing {
                    relations
                        .update_kind(rel.id, daruma_domain::RelationKind::WasBlocking)
                        .await?;
                    events.push(Event::TaskRelationKindChanged {
                        relation_id: rel.id,
                        from: rel.from,
                        to: rel.to,
                        from_kind: daruma_domain::RelationKind::Blocks,
                        to_kind: daruma_domain::RelationKind::WasBlocking,
                        occurred_at: now,
                    });
                }
            }
        }

        Ok(events)
    }

    /// Return the first active run_id for a plan, if any.
    async fn active_run_for_plan(&self, plan_id: PlanId) -> Option<RunId> {
        let run_repo = self.runs.as_ref()?;
        run_repo
            .list_active_for_plan(plan_id)
            .await
            .ok()?
            .into_iter()
            .next()
            .map(|r| r.id)
    }

    /// §3.7.4 — liveness watchdog tick. For each active run that breaches the
    /// ack/idle thresholds, emit `RunUnresponsive` / `RunStale` exactly once
    /// (subsequent ticks skip rows where the corresponding `*_at` column is
    /// already non-null). Run status is *not* changed; this is signal-only.
    ///
    /// Returns the total number of liveness events emitted on this tick.
    pub async fn tick_liveness(
        &self,
        now: daruma_shared::Timestamp,
        ack_secs: u64,
        idle_secs: u64,
    ) -> Result<usize> {
        let Some(runs) = self.runs.as_ref() else {
            return Ok(0);
        };

        let mut emitted = 0usize;

        let unresponsive = runs
            .list_unresponsive_candidates(std::time::Duration::from_secs(ack_secs), now)
            .await?;
        for run_id in unresponsive {
            self.persist_signal_event(Event::RunUnresponsive { run_id, at: now })
                .await?;
            emitted += 1;
        }

        let stale = runs
            .list_stale_candidates(std::time::Duration::from_secs(idle_secs), now)
            .await?;
        for run_id in stale {
            self.persist_signal_event(Event::RunStale { run_id, at: now })
                .await?;
            emitted += 1;
        }

        Ok(emitted)
    }

    /// Due-date watchdog: emit `TaskDueElapsed` (webhook kind `task.due`)
    /// for every active task whose `due_at` has passed and that has not
    /// been notified for this deadline value yet. Returns the number of
    /// events emitted. Idempotent per (task, due_at): the projection in
    /// `task_due_notifications` dedupes across ticks and restarts.
    pub async fn tick_due_tasks(&self, now: daruma_shared::Timestamp) -> Result<usize> {
        let due = self.tasks.list_due_unnotified(now, 100).await?;
        let mut emitted = 0usize;
        for (task_id, due_at) in due {
            self.persist_signal_event(Event::TaskDueElapsed {
                task_id,
                due_at,
                at: now,
            })
            .await?;
            emitted += 1;
        }
        Ok(emitted)
    }

    /// Route freshly persisted events into the project's auto-created
    /// `Interview` (agent activity) / `Human Log` (human milestones)
    /// documents, honoring the per-project [`AutoAppendSettings`] toggles
    /// (ON by default). Best-effort: failures are logged, never propagated,
    /// so observability cannot break a command. Document and settings
    /// events themselves are excluded to prevent recursion.
    ///
    /// [`AutoAppendSettings`]: daruma_domain::AutoAppendSettings
    async fn auto_append_logs(&self, persisted: &[EventEnvelope]) {
        let Some(documents) = &self.documents else {
            return;
        };
        for env in persisted {
            let Some((project_id, kind, line)) = self.render_log_line(env).await else {
                continue;
            };
            let enabled = match &self.project_settings {
                Some(repo) => {
                    let settings = repo.auto_append(project_id).await.unwrap_or_default();
                    match kind {
                        DocumentKind::Interview => settings.interview,
                        DocumentKind::HumanLog => settings.human_log,
                    }
                }
                None => true,
            };
            if !enabled {
                continue;
            }
            let doc = match documents
                .list_by_project(project_id, Some(kind), false)
                .await
            {
                // The auto-created log is the oldest doc of its kind.
                Ok(docs) => docs.into_iter().next(),
                Err(e) => {
                    tracing::warn!(err = %e, "auto-append: document lookup failed");
                    continue;
                }
            };
            let Some(doc) = doc else { continue };
            if let Err(e) = self
                .persist_signal_event(Event::DocumentContentAppended {
                    document_id: doc.id,
                    append: line,
                    at: env.occurred_at,
                })
                .await
            {
                tracing::warn!(err = %e, "auto-append: write failed");
            }
        }
    }

    /// Decide whether an event lands in a log, and render the line.
    /// `Interview` lines are machine-ish (`[ts] agent=… action=… target=…`);
    /// `Human Log` lines are plain prose with a short local timestamp.
    async fn render_log_line(
        &self,
        env: &EventEnvelope,
    ) -> Option<(ProjectId, DocumentKind, String)> {
        use daruma_domain::Actor as A;
        let ts_iso = env.occurred_at.format("%Y-%m-%dT%H:%M:%SZ");
        let ts_human = env.occurred_at.format("%Y-%m-%d %H:%M");
        let agent_name = match &env.actor {
            A::Agent { name, .. } => name.clone(),
            A::User => "user".to_string(),
        };

        match &env.payload {
            Event::TaskCreated { task } => {
                let project_id = task.project_id?;
                let title = task.title.trim();
                if env.actor.is_agent() {
                    Some((
                        project_id,
                        DocumentKind::Interview,
                        format!(
                            "[{ts_iso}] agent={agent_name} action=task_created target={} \"{title}\"",
                            task.id.unwrap_or_default()
                        ),
                    ))
                } else {
                    Some((
                        project_id,
                        DocumentKind::HumanLog,
                        format!("{ts_human} — Created task '{title}'"),
                    ))
                }
            }
            Event::TaskStatusChanged { task_id, from, to } => {
                let task = self.tasks.get(*task_id).await.ok().flatten()?;
                let project_id = task.project_id?;
                if env.actor.is_agent() {
                    Some((
                        project_id,
                        DocumentKind::Interview,
                        format!(
                            "[{ts_iso}] agent={agent_name} action=status_changed target={task_id} {from:?}->{to:?}"
                        ),
                    ))
                } else {
                    Some((
                        project_id,
                        DocumentKind::HumanLog,
                        format!(
                            "{ts_human} — Task '{}' status: {from:?} → {to:?}",
                            task.title.trim()
                        ),
                    ))
                }
            }
            Event::RunStarted { run } => {
                let project_id = self.project_for_plan(run.plan_id).await?;
                Some((
                    project_id,
                    DocumentKind::Interview,
                    format!(
                        "[{ts_iso}] agent={agent_name} action=run_started target={} plan={}",
                        run.id, run.plan_id
                    ),
                ))
            }
            Event::RunCompleted { run_id, .. } => {
                let project_id = self.project_for_run(*run_id).await?;
                Some((
                    project_id,
                    DocumentKind::Interview,
                    format!("[{ts_iso}] agent={agent_name} action=run_completed target={run_id}"),
                ))
            }
            Event::RunAborted { run_id, reason, .. } => {
                let project_id = self.project_for_run(*run_id).await?;
                Some((
                    project_id,
                    DocumentKind::Interview,
                    format!(
                        "[{ts_iso}] agent={agent_name} action=run_aborted target={run_id} reason={reason}"
                    ),
                ))
            }
            Event::RunNoteAppended { run_id, body, .. } => {
                let project_id = self.project_for_run(*run_id).await?;
                let body = body.trim().replace('\n', " ");
                let preview: String = body.chars().take(120).collect();
                Some((
                    project_id,
                    DocumentKind::Interview,
                    format!(
                        "[{ts_iso}] agent={agent_name} action=run_note target={run_id} {preview}"
                    ),
                ))
            }
            Event::PlanStatusChanged { plan_id, to, .. }
                if *to == daruma_domain::PlanStatus::Completed =>
            {
                let plan = self.plans.as_ref()?.get(*plan_id).await.ok().flatten()?;
                Some((
                    plan.project_id,
                    DocumentKind::HumanLog,
                    format!("{ts_human} — Plan '{}' completed", plan.title.trim()),
                ))
            }
            Event::ProjectUpdated {
                project_id,
                title: Some(title),
                ..
            } => Some((
                *project_id,
                DocumentKind::HumanLog,
                format!("{ts_human} — Project renamed to '{}'", title.trim()),
            )),
            _ => None,
        }
    }

    async fn work_unit(&self, id: daruma_shared::WorkUnitId) -> Result<daruma_domain::WorkUnit> {
        let repo = self
            .work_units
            .as_ref()
            .ok_or_else(|| CoreError::storage("work unit repository not configured"))?;
        repo.get(id)
            .await?
            .ok_or_else(|| CoreError::not_found(format!("work unit {id}")))
    }

    async fn project_for_plan(&self, plan_id: PlanId) -> Option<ProjectId> {
        let plan = self.plans.as_ref()?.get(plan_id).await.ok().flatten()?;
        Some(plan.project_id)
    }

    async fn project_for_run(&self, run_id: RunId) -> Option<ProjectId> {
        let run = self.runs.as_ref()?.get(run_id).await.ok().flatten()?;
        self.project_for_plan(run.plan_id).await
    }

    /// Public wrapper over [`Self::persist_signal_event`] for transports
    /// that push system-authored progress signals (e.g. §3.8.12 async AI
    /// operation events on `Channel::AiOps`).
    pub async fn emit_system_event(&self, payload: Event) -> Result<()> {
        self.persist_signal_event(payload).await
    }

    /// Persist a single system-authored event (no command validation), apply
    /// it to all projections, and publish on the bus. Used by background
    /// signals such as the liveness watchdog (§3.7.4).
    async fn persist_signal_event(&self, payload: Event) -> Result<()> {
        self.persist_signal_event_as(Actor::user(), payload).await
    }

    /// [`Self::persist_signal_event`] with an explicit actor (e.g. the
    /// claiming agent for work-unit dispatch events).
    pub async fn emit_system_event_as(&self, actor: Actor, payload: Event) -> Result<()> {
        self.persist_signal_event_as(actor, payload).await
    }

    async fn persist_signal_event_as(&self, actor: Actor, payload: Event) -> Result<()> {
        let envelope = EventEnvelope::new(actor, payload);
        let persisted = self.store.append_batch(vec![envelope]).await?;
        for env in &persisted {
            self.tasks.apply_event(env).await?;
            self.projects.apply_event(env).await?;
            self.comments.apply_event(env).await?;
            self.activity.apply_event(env).await?;
            if let Some(relations) = &self.relations {
                relations.apply_event(&env.payload).await;
            }
            if let Some(plans) = &self.plans {
                plans.apply_event(env).await?;
            }
            if let Some(runs) = &self.runs {
                runs.apply_event(env).await?;
            }
            if let Some(run_notes) = &self.run_notes {
                run_notes.apply_event(env).await?;
            }
            if let Some(sessions) = &self.sessions {
                sessions.apply_event(env).await?;
            }
            if let Some(claims) = &self.claims {
                claims.apply_event(env).await?;
            }
            if let Some(work_leases) = &self.work_leases {
                work_leases.apply_event(env).await?;
            }
            if let Some(ext) = &self.external_refs {
                ext.apply_event(env).await?;
            }
            if let Some(documents) = &self.documents {
                documents.apply_event(env).await?;
            }
            if let Some(settings) = &self.project_settings {
                settings.apply_event(env).await?;
            }
            // Capability profiles read the unit's owner, which the work-unit
            // projector clears on completion/release — so profiles fold their
            // signal first (P6).
            if let Some(profiles) = &self.capability_profiles {
                profiles.apply_event(env).await?;
            }
            if let Some(work_units) = &self.work_units {
                work_units.apply_event(env).await?;
            }
            if let Some(handoffs) = &self.handoffs {
                handoffs.apply_event(env).await?;
            }
            if let Some(rules) = &self.rules {
                rules.apply_event(env).await?;
            }
            if let Some(evidence) = &self.evidence {
                evidence.apply_event(env).await?;
            }
            self.spawn_search_index(env.clone());
            self.bus.publish(env.clone());
        }
        Ok(())
    }

    fn spawn_search_index(&self, env: EventEnvelope) {
        let Some(provider) = &self.search_provider else {
            return;
        };

        let provider = Arc::clone(provider);
        let tasks = Arc::clone(&self.tasks);
        let comments = Arc::clone(&self.comments);
        tokio::spawn(async move {
            match index_items_for_event(&env, &tasks, &comments).await {
                Ok(items) => {
                    for item in items {
                        if let Err(err) = provider.index(item).await {
                            tracing::warn!(
                                err = %err,
                                event_id = %env.id,
                                event_seq = env.seq,
                                "search index update failed"
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        err = %err,
                        event_id = %env.id,
                        event_seq = env.seq,
                        "search index item extraction failed"
                    );
                }
            }
        });
    }

    // ── private: command → event list ─────────────────────────────────────────

    async fn build_events(&self, cmd: Command, actor: &Actor) -> Result<Vec<Event>> {
        match cmd {
            // ── Task commands ─────────────────────────────────────────────────
            Command::CreateTask { mut task } => {
                let title = task.title.trim().to_string();
                if title.is_empty() {
                    return Err(CoreError::validation("task title must not be empty"));
                }
                if title.len() > 500 {
                    return Err(CoreError::validation(
                        "task title must not exceed 500 characters",
                    ));
                }
                task.title = title;
                if task.id.is_none() {
                    task.id = Some(TaskId::new());
                }
                if let Some(quotas) = &self.tenant_quotas {
                    quotas.check_task_quota(task.project_id).await?;
                }
                Ok(vec![Event::TaskCreated { task }])
            }

            Command::UpdateTask { id, patch } => {
                if patch.is_empty() {
                    return Err(CoreError::validation("update patch must not be empty"));
                }
                self.tasks
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
                Ok(vec![Event::TaskUpdated { task_id: id, patch }])
            }

            Command::CompleteTask { id, note } => {
                let task = self
                    .tasks
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
                if task.status == Status::Done {
                    return Err(CoreError::conflict("task already done"));
                }

                // Blocker check (W2.2): reject if any active blocker exists.
                if let Some(relations) = &self.relations {
                    let blocked_by =
                        relation_enforcement::list_active_blockers(relations, &self.tasks, id)
                            .await?;
                    if !blocked_by.is_empty() {
                        return Err(CoreError::conflict(format!(
                            "task_blocked: blocked by {} task(s): {:?}",
                            blocked_by.len(),
                            blocked_by
                        )));
                    }
                }

                let from = task.status;
                let now = time::now();
                // Stamp the completing actor onto the note (human vs agent) so
                // the audit trail can tell a human-verified completion from an
                // agent-self-reported one. Empty notes carry no payload — only
                // a substantive note (or one with a meaningful actor) rides the
                // event; a bare `Some(default)` collapses to `None`.
                let completion_note = note.map(|mut n| {
                    n.actor = Some(daruma_domain::ActorRef::from_actor(actor));
                    n
                });
                let mut events = vec![
                    Event::TaskStatusChanged {
                        task_id: id,
                        from,
                        to: Status::Done,
                    },
                    Event::TaskCompleted {
                        task_id: id,
                        completed_at: now,
                        completion_note,
                    },
                    Event::TaskClosed {
                        task_id: id,
                        by: actor.clone(),
                        at: now,
                    },
                ];

                // Downstream unblock (W2.2): emit TaskUnblocked for tasks that
                // become fully unblocked now that `id` is transitioning to Done.
                if let Some(relations) = &self.relations {
                    let to_unblock = relation_enforcement::list_downstream_to_unblock(
                        relations,
                        &self.tasks,
                        id,
                    )
                    .await?;
                    for task_id in to_unblock {
                        events.push(Event::TaskUnblocked {
                            task_id,
                            unblocked_by: id,
                            occurred_at: now,
                        });
                    }

                    // §3.7.2 / LIN A.3: transition every active `Blocks` edge
                    // outgoing from `id` to `WasBlocking` — historical retention
                    // of the resolved dependency.
                    let outgoing = relations.list_blocks_targets(id).await?;
                    for rel in outgoing {
                        relations
                            .update_kind(rel.id, daruma_domain::RelationKind::WasBlocking)
                            .await?;
                        events.push(Event::TaskRelationKindChanged {
                            relation_id: rel.id,
                            from: rel.from,
                            to: rel.to,
                            from_kind: daruma_domain::RelationKind::Blocks,
                            to_kind: daruma_domain::RelationKind::WasBlocking,
                            occurred_at: now,
                        });
                    }
                }

                Ok(events)
            }

            Command::DeleteTask { id } => {
                self.tasks
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;

                let now = time::now();
                let mut events: Vec<Event> = Vec::new();

                // Cascade (W2.2 R7): emit TaskUnlinked per relation + TaskUnblocked
                // for downstreams where `id` was the last active blocker.
                if let Some(relations) = &self.relations {
                    let all_relations = relations.list_by_task(id).await?;

                    // Compute which downstreams will become unblocked once `id` is
                    // removed. We must do this BEFORE deleting rows so that
                    // list_blockers still sees `id` and we can skip it correctly.
                    let to_unblock = relation_enforcement::list_downstream_to_unblock(
                        relations,
                        &self.tasks,
                        id,
                    )
                    .await?;

                    // Emit TaskUnlinked for every relation touching this task.
                    for rel in &all_relations {
                        events.push(Event::TaskUnlinked {
                            relation_id: rel.id,
                            from: rel.from,
                            to: rel.to,
                            kind: rel.kind,
                            occurred_at: now,
                        });
                        // Delete each row so the projection stays consistent.
                        relations.delete(rel.id).await?;
                    }

                    // Emit TaskUnblocked for tasks that become free.
                    for task_id in to_unblock {
                        events.push(Event::TaskUnblocked {
                            task_id,
                            unblocked_by: id,
                            occurred_at: now,
                        });
                    }
                }

                // Cascade (§3.2.6): emit PlanTaskRemoved for every plan that
                // contains this task, so plan progress and plan_next_task stay
                // consistent after deletion.
                // TODO(ROADMAP §3.2.6, task 019e31bf): add a SQL backfill migration
                // for any pre-existing dangling plan_tasks rows created before this
                // cascade was introduced.
                if let Some(plan_repo) = &self.plans {
                    let plan_ids = plan_repo.list_plans_for_task(id).await?;
                    for plan_id in plan_ids {
                        events.push(Event::PlanTaskRemoved {
                            plan_id,
                            task_id: id,
                        });
                    }
                }

                events.push(Event::TaskDeleted { task_id: id });
                Ok(events)
            }

            Command::SetStatus { id, status, .. } => {
                let task = self
                    .tasks
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
                self.emit_status_transition_events(&task, status, actor, time::now())
                    .await
            }

            Command::SetPriority { id, priority } => {
                let task = self
                    .tasks
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
                if task.priority == priority {
                    return Ok(vec![]); // no-op
                }
                Ok(vec![Event::TaskPriorityChanged {
                    task_id: id,
                    from: task.priority,
                    to: priority,
                }])
            }

            // ── Bulk task commands (§3.7.7 / LIN B.7) ─────────────────────────
            Command::BulkSetStatus { ids, status } => {
                let unique = dedupe_ids(&ids);
                validate_bulk_cap(unique.len())?;

                // Pre-load all tasks; fail-fast if any id is missing.
                let mut tasks = Vec::with_capacity(unique.len());
                let mut missing: Vec<TaskId> = Vec::new();
                for id in &unique {
                    match self.tasks.get(*id).await? {
                        Some(t) => tasks.push(t),
                        None => missing.push(*id),
                    }
                }
                if !missing.is_empty() {
                    return Err(CoreError::not_found(format!(
                        "bulk_set_status: {} task(s) not found: {:?}",
                        missing.len(),
                        missing
                    )));
                }

                let now = time::now();
                let mut events: Vec<Event> = Vec::new();
                for task in tasks {
                    events.extend(
                        self.emit_status_transition_events(&task, status, actor, now)
                            .await?,
                    );
                }

                Ok(events)
            }

            Command::BulkAttachToPlan { plan_id, task_ids } => {
                let unique = dedupe_ids(&task_ids);
                validate_bulk_cap(unique.len())?;

                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                // Verify every task exists; fail-fast otherwise.
                let mut missing: Vec<TaskId> = Vec::new();
                for id in &unique {
                    if self.tasks.get(*id).await?.is_none() {
                        missing.push(*id);
                    }
                }
                if !missing.is_empty() {
                    return Err(CoreError::not_found(format!(
                        "bulk_attach_to_plan: {} task(s) not found: {:?}",
                        missing.len(),
                        missing
                    )));
                }

                // Skip ids that are already attached so this is idempotent.
                let existing = plan_repo.list_plan_tasks_ordered(plan_id).await?;
                let already: std::collections::HashSet<TaskId> =
                    existing.iter().map(|t| t.task_id).collect();
                let mut next_pos = existing.last().map_or(0, |t| t.position + 1);

                let active_run_id = self.active_run_for_plan(plan_id).await;
                let mut events: Vec<Event> = Vec::with_capacity(unique.len() + 1);
                let mut any_added = false;
                for id in unique {
                    if already.contains(&id) {
                        continue;
                    }
                    events.push(Event::PlanTaskAdded {
                        plan_id,
                        task_id: id,
                        position: next_pos,
                        depends_on: Vec::new(),
                    });
                    next_pos += 1;
                    any_added = true;
                }
                if any_added {
                    events.push(Event::PlanModifiedByHuman {
                        plan_id,
                        during_run_id: active_run_id,
                    });
                }
                Ok(events)
            }

            Command::SplitTask { parent, subtasks } => {
                if subtasks.len() < 2 {
                    return Err(CoreError::validation("split requires at least 2 subtasks"));
                }
                let parent_task = self
                    .tasks
                    .get(parent)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {parent}")))?;

                let prepared: Vec<_> = subtasks
                    .into_iter()
                    .map(|mut st| {
                        if st.id.is_none() {
                            st.id = Some(TaskId::new());
                        }
                        if st.project_id.is_none() {
                            st.project_id = parent_task.project_id;
                        }
                        st
                    })
                    .collect();

                let mut events = vec![Event::TaskSplitGenerated {
                    parent,
                    subtasks: prepared.clone(),
                }];
                for st in prepared {
                    events.push(Event::TaskCreated { task: st });
                }
                Ok(events)
            }

            // ── Project commands ──────────────────────────────────────────────
            Command::CreateProject { title, description } => {
                let trimmed = title.trim().to_string();
                if trimmed.is_empty() {
                    return Err(CoreError::validation("project title must not be empty"));
                }
                let base_slug = daruma_domain::slugify_title(&trimmed);
                let existing = self.projects.list_all().await?;
                let mut slug = base_slug.clone();
                let mut suffix = 0u32;
                while existing.iter().any(|p| p.slug == slug) {
                    suffix += 1;
                    slug = format!("{base_slug}-{suffix}");
                }
                let project = Project::new_with_slug(trimmed, slug, description);

                // Execution-layer projects are created bare: no narrative
                // documents are seeded. The `Document` primitive remains a
                // structured task-artifact store (see `doc_create`/`doc_list`);
                // narrative Interview / Human Log default docs are a
                // product concern (Intake / Sensemaking) and are no longer
                // auto-created by the core on project creation.
                Ok(vec![Event::ProjectCreated { project }])
            }

            Command::UpdateProject {
                id,
                title,
                description,
            } => {
                if title.is_none() && description.is_none() {
                    return Err(CoreError::validation("update must set at least one field"));
                }
                self.projects
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("project {id}")))?;
                Ok(vec![Event::ProjectUpdated {
                    project_id: id,
                    title,
                    description,
                }])
            }

            Command::UpdateProjectSettings {
                project_id,
                auto_append,
            } => {
                self.projects
                    .get(project_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("project {project_id}")))?;
                let repo = self.project_settings.as_ref().ok_or_else(|| {
                    CoreError::storage("project settings repository not configured")
                })?;
                let current = repo.auto_append(project_id).await?;
                Ok(vec![Event::ProjectSettingsChanged {
                    project_id,
                    auto_append: current.apply(auto_append),
                    at: daruma_shared::time::now(),
                }])
            }

            Command::CreateWorkUnit { work_unit } => {
                self.tasks
                    .get(work_unit.task_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {}", work_unit.task_id)))?;
                let mut wu = work_unit.into_work_unit(time::now());
                if wu.id == daruma_shared::WorkUnitId::default() {
                    wu.id = daruma_shared::WorkUnitId::new();
                }
                if wu.title.trim().is_empty() {
                    return Err(CoreError::validation("work unit title must not be empty"));
                }
                Ok(vec![Event::WorkUnitCreated { work_unit: wu }])
            }

            Command::CompleteWorkUnit {
                id,
                outcome,
                produced_artifacts,
                next_suggested_units,
            } => {
                let unit = self.work_unit(id).await?;
                if unit.status.is_terminal() {
                    return Err(CoreError::conflict(format!(
                        "work unit {id} already closed"
                    )));
                }
                Ok(vec![Event::WorkUnitCompleted {
                    work_unit_id: id,
                    outcome: outcome.unwrap_or_else(|| "ok".into()),
                    produced_artifacts,
                    next_suggested_units,
                    at: time::now(),
                }])
            }

            Command::ReleaseWorkUnit { id } => {
                self.work_unit(id).await?;
                Ok(vec![Event::WorkUnitReleased {
                    work_unit_id: id,
                    at: time::now(),
                }])
            }

            Command::SetWorkUnitStatus { id, status, reason } => {
                use daruma_domain::WorkUnitStatus as WUS;
                let unit = self.work_unit(id).await?;
                if unit.status.is_terminal() {
                    return Err(CoreError::conflict(format!(
                        "work unit {id} already closed"
                    )));
                }
                let now = time::now();
                match status {
                    WUS::InProgress => Ok(vec![Event::WorkUnitStarted {
                        work_unit_id: id,
                        at: now,
                    }]),
                    WUS::Blocked => Ok(vec![Event::WorkUnitBlocked {
                        work_unit_id: id,
                        reason: reason.unwrap_or_else(|| "blocked".into()),
                        at: now,
                    }]),
                    WUS::Done => Ok(vec![Event::WorkUnitCompleted {
                        work_unit_id: id,
                        outcome: "ok".into(),
                        produced_artifacts: vec![],
                        next_suggested_units: vec![],
                        at: now,
                    }]),
                    WUS::Ready | WUS::Todo => Ok(vec![Event::WorkUnitReleased {
                        work_unit_id: id,
                        at: now,
                    }]),
                    other => Err(CoreError::validation(format!(
                        "unsupported work unit transition to {other:?}                          (review/cancelled land with the handoff layer)"
                    ))),
                }
            }

            Command::DeleteProject { id } => {
                self.projects
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("project {id}")))?;
                let tasks = self.tasks.list_by_project(Some(id)).await?;
                if !tasks.is_empty() {
                    return Err(CoreError::conflict(format!(
                        "project_not_empty: {} task(s) attached",
                        tasks.len()
                    )));
                }
                // Plan-emptiness check lives in the HTTP route, which has
                // direct access to the concrete `PlanRepo` (the core trait
                // does not expose a list-by-project helper).
                Ok(vec![Event::ProjectDeleted { project_id: id }])
            }

            // ── Agent commands ────────────────────────────────────────────────
            Command::RecordAgentAction { action } => {
                Ok(vec![Event::AgentActionRecorded { action }])
            }

            // ── Comment commands ──────────────────────────────────────────────
            Command::AddComment {
                comment: new_comment,
            } => {
                let body = new_comment.body.trim().to_string();
                if body.is_empty() {
                    return Err(CoreError::validation("comment body must not be empty"));
                }
                if body.len() > 10_000 {
                    return Err(CoreError::validation(
                        "comment body must not exceed 10000 characters",
                    ));
                }
                let now = time::now();
                let preview: String = body.chars().take(80).collect();
                let comment = Comment::from_new(
                    daruma_domain::NewComment {
                        body,
                        ..new_comment
                    },
                    actor.clone(),
                    now,
                );
                let task_id = comment.task_id;
                let comment_id = comment.id;
                Ok(vec![
                    Event::CommentAdded { comment },
                    Event::TaskCommented {
                        task_id,
                        comment_id,
                        author: actor.clone(),
                        preview,
                    },
                ])
            }

            Command::EditComment { id, patch } => {
                let comment = self
                    .comments
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("comment {id}")))?;
                if comment.deleted_at.is_some() {
                    return Err(CoreError::not_found(format!("comment {id}")));
                }
                let edited_at = time::now();
                Ok(vec![Event::CommentEdited {
                    comment_id: id,
                    task_id: comment.task_id,
                    patch,
                    edited_at,
                }])
            }

            Command::DeleteComment { id } => {
                let comment = self
                    .comments
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("comment {id}")))?;
                if comment.deleted_at.is_some() {
                    return Err(CoreError::not_found(format!("comment {id}")));
                }
                let deleted_at = time::now();
                Ok(vec![Event::CommentDeleted {
                    comment_id: id,
                    task_id: comment.task_id,
                    deleted_at,
                }])
            }

            // ── Plan commands (W2.2) ──────────────────────────────────────────
            Command::CreatePlan {
                mut plan,
                external_ref,
            } => {
                // Idempotent create via external_ref
                if let Some((ref tenant, ref kind, ref ext_id)) = external_ref {
                    if let Some(ext_repo) = &self.external_refs {
                        if ext_repo.lookup(tenant, kind, ext_id).await?.is_some() {
                            return Ok(vec![]); // already created — idempotent no-op
                        }
                    }
                }

                // Validate title
                let title = plan.title.trim().to_string();
                if title.is_empty() {
                    return Err(CoreError::validation("plan title must not be empty"));
                }
                if title.len() > 500 {
                    return Err(CoreError::validation(
                        "plan title must not exceed 500 characters",
                    ));
                }
                plan.title = title;
                if let Some(quotas) = &self.tenant_quotas {
                    quotas.check_plan_quota(plan.project_id).await?;
                }

                let id = PlanId::new();

                // Validate parent cycle
                if let Some(parent_id) = plan.parent_plan_id {
                    if let Some(plan_repo) = &self.plans {
                        detect_parent_cycle(plan_repo.as_ref(), id, parent_id).await?;
                    }
                }

                let now = time::now();
                let new_plan = plan.into_plan(id, now);
                Ok(vec![Event::PlanCreated { plan: new_plan }])
            }

            Command::UpdatePlan { id, patch } => {
                if patch.is_empty() {
                    return Err(CoreError::validation("update patch must not be empty"));
                }
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                let plan = plan_repo
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {id}")))?;

                // Cycle-detect when re-parenting (self-reference and ancestor cycles)
                if let Some(Some(new_parent)) = patch.parent_plan_id {
                    detect_parent_cycle(plan_repo.as_ref(), id, new_parent).await?;
                }

                let mut events = vec![];

                // Emit PlanGoalChanged alongside PlanUpdated when goal changes
                if let Some(new_goal) = &patch.goal {
                    if *new_goal != plan.goal {
                        events.push(Event::PlanGoalChanged {
                            plan_id: id,
                            from: plan.goal.clone(),
                            to: new_goal.clone(),
                        });
                    }
                }

                events.push(Event::PlanUpdated { plan_id: id, patch });
                Ok(events)
            }

            Command::ArchivePlan { id } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                let plan = plan_repo
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {id}")))?;

                if plan.archived_at.is_some() {
                    return Ok(vec![]); // already archived — no-op
                }

                let now = time::now();
                let mut events = vec![Event::PlanArchived {
                    plan_id: id,
                    at: now,
                }];

                // Atomically abort all active runs (§3.7)
                if let Some(run_repo) = &self.runs {
                    let active_runs = run_repo.list_active_for_plan(id).await?;
                    for run in active_runs {
                        events.push(Event::RunAborted {
                            run_id: run.id,
                            reason: "plan_archived".to_string(),
                            at: now,
                        });
                        events.push(Event::RunObsolescedByPlanEdit {
                            run_id: run.id,
                            plan_id: id,
                            kind: ObsolescenceKind::Archived,
                        });
                    }
                }

                Ok(events)
            }

            Command::AddPlanTask {
                plan_id,
                task_id,
                position,
                depends_on,
            } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                self.tasks
                    .get(task_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("task {task_id}")))?;

                let pos = match position {
                    Some(p) => p,
                    None => {
                        let existing = plan_repo.list_plan_tasks_ordered(plan_id).await?;
                        existing.last().map_or(0, |t| t.position + 1)
                    }
                };

                let active_run_id = self.active_run_for_plan(plan_id).await;

                Ok(vec![
                    Event::PlanTaskAdded {
                        plan_id,
                        task_id,
                        position: pos,
                        depends_on: depends_on.unwrap_or_default(),
                    },
                    Event::PlanModifiedByHuman {
                        plan_id,
                        during_run_id: active_run_id,
                    },
                ])
            }

            Command::RemovePlanTask { plan_id, task_id } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                let now = time::now();
                let mut events = vec![Event::PlanTaskRemoved { plan_id, task_id }];

                // Check if the task is the current step of any active run — if so,
                // emit TaskContested + RunStepFinished{Superseded} (§3.7)
                let contested_run = if let Some(run_repo) = &self.runs {
                    let active_runs = run_repo.list_active_for_plan(plan_id).await?;
                    let mut found: Option<(RunId, AgentId)> = None;
                    for run in active_runs {
                        if run_repo.current_step_task(run.id).await? == Some(task_id) {
                            found = Some((run.id, run.agent_id));
                            break;
                        }
                    }
                    found
                } else {
                    None
                };

                if let Some((run_id, agent_id)) = contested_run {
                    events.push(Event::TaskContested {
                        task_id,
                        actors: vec![actor.clone(), Actor::agent(agent_id.to_string())],
                        field: None,
                    });
                    events.push(Event::RunStepFinished {
                        run_id,
                        task_id,
                        outcome: RunOutcome::Superseded,
                        at: now,
                    });
                }

                Ok(events)
            }

            Command::ReorderPlan { plan_id, order } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                let active_run_id = self.active_run_for_plan(plan_id).await;

                Ok(vec![
                    Event::PlanReordered { plan_id, order },
                    Event::PlanModifiedByHuman {
                        plan_id,
                        during_run_id: active_run_id,
                    },
                ])
            }

            Command::SetPlanGoal { plan_id, goal } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                let plan = plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                if plan.goal == goal {
                    return Ok(vec![]); // no-op
                }

                let from = plan.goal.clone();
                Ok(vec![
                    Event::PlanGoalChanged {
                        plan_id,
                        from,
                        to: goal.clone(),
                    },
                    Event::PlanUpdated {
                        plan_id,
                        patch: PlanPatch {
                            goal: Some(goal),
                            ..Default::default()
                        },
                    },
                ])
            }

            Command::SetPlanStatus { plan_id, status } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                let plan = plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                if plan.status == status {
                    return Ok(vec![]); // no-op
                }

                if status == PlanStatus::Completed {
                    let plan_tasks = plan_repo.list_plan_tasks_ordered(plan_id).await?;
                    for pt in &plan_tasks {
                        if let Some(task) = self.tasks.get(pt.task_id).await? {
                            if !task.status.is_terminal() {
                                return Err(CoreError::validation(
                                    "cannot complete plan: it has unfinished tasks (move them to done/cancelled first)",
                                ));
                            }
                        }
                    }
                }

                Ok(vec![Event::PlanStatusChanged {
                    plan_id,
                    from: plan.status,
                    to: status,
                }])
            }

            // ── Run commands ──────────────────────────────────────────────────
            Command::StartRun {
                plan_id,
                agent_id,
                parent_run_id,
            } => {
                let plan_repo = self
                    .plans
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("plan repository not configured"))?;
                let plan = plan_repo
                    .get(plan_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("plan {plan_id}")))?;

                if plan.status != PlanStatus::Active {
                    return Err(CoreError::validation("plan must be Active to start a run"));
                }

                let now = time::now();
                let run = Run {
                    id: RunId::new(),
                    plan_id,
                    agent_id,
                    parent_run_id,
                    started_at: now,
                    ended_at: None,
                    status: RunStatus::Active,
                    outcome: None,
                    last_activity_at: Some(now),
                    unresponsive_at: None,
                    stale_at: None,
                };
                Ok(vec![Event::RunStarted { run }])
            }

            Command::RunStartStep { run_id, task_id } => {
                let run_repo = self
                    .runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?;
                let run = run_repo
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                if run.status != RunStatus::Active {
                    return Err(CoreError::validation("run is not active"));
                }

                let now = time::now();
                Ok(vec![Event::RunStepStarted {
                    run_id,
                    task_id,
                    at: now,
                }])
            }

            Command::RunFinishStep {
                run_id,
                task_id,
                outcome,
            } => {
                self.runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                let now = time::now();
                Ok(vec![Event::RunStepFinished {
                    run_id,
                    task_id,
                    outcome,
                    at: now,
                }])
            }

            Command::CompleteRun { run_id } => {
                let run = self
                    .runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                if run.status != RunStatus::Active {
                    return Err(CoreError::conflict("run is not active"));
                }

                let now = time::now();
                Ok(vec![Event::RunCompleted { run_id, at: now }])
            }

            Command::FailRun { run_id, reason } => {
                self.runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                let now = time::now();
                Ok(vec![Event::RunFailed {
                    run_id,
                    reason,
                    at: now,
                }])
            }

            Command::AbortRun { run_id, reason } => {
                self.runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                let now = time::now();
                Ok(vec![Event::RunAborted {
                    run_id,
                    reason,
                    at: now,
                }])
            }

            // ── Run notes (§3.8.2) ────────────────────────────────────────────
            Command::AppendRunNote { run_id, body } => {
                // body validation: non-empty after trim, ≤ 4 KiB raw bytes.
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    return Err(CoreError::validation("run note body must not be empty"));
                }
                if body.len() > RUN_NOTE_MAX_BYTES {
                    return Err(CoreError::validation(format!(
                        "run note body must not exceed {RUN_NOTE_MAX_BYTES} bytes",
                    )));
                }

                let run = self
                    .runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                if run.status != RunStatus::Active {
                    return Err(CoreError::validation(
                        "cannot append a note to a terminal run",
                    ));
                }

                let now = time::now();
                Ok(vec![Event::RunNoteAppended {
                    run_id,
                    note_id: RunNoteId::new(),
                    body,
                    by_actor: actor.clone(),
                    at: now,
                }])
            }

            // ── Agent session commands ────────────────────────────────────────
            Command::StartAgentSession {
                agent_id,
                parent_agent_id,
                metadata,
            } => {
                let now = time::now();
                let session = AgentSession {
                    id: AgentSessionId::new(),
                    agent_id,
                    parent_agent_id,
                    started_at: now,
                    ended_at: None,
                    plan_steps: vec![],
                    metadata: metadata.unwrap_or(serde_json::Value::Null),
                };
                Ok(vec![Event::AgentSessionStarted { session }])
            }

            Command::EndAgentSession { id } => {
                self.sessions
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("session repository not configured"))?
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("session {id}")))?;

                let now = time::now();
                Ok(vec![Event::AgentSessionEnded {
                    session_id: id,
                    at: now,
                }])
            }

            Command::UpdateAgentSessionPlan { id, steps } => {
                // Validate: Linear B.1 — max 100 steps
                if steps.len() > 100 {
                    return Err(CoreError::validation(
                        "plan_steps must not exceed 100 entries",
                    ));
                }

                self.sessions
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("session repository not configured"))?
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("session {id}")))?;

                Ok(vec![Event::AgentSessionPlanUpdated {
                    session_id: id,
                    steps,
                }])
            }

            Command::AttachSessionArtifact {
                session_id,
                kind,
                reference,
                metadata,
            } => {
                let reference = reference.trim().to_string();
                if reference.is_empty() {
                    return Err(CoreError::validation("artifact ref must not be empty"));
                }
                self.sessions
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("session repository not configured"))?
                    .get(session_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("session {session_id}")))?;

                let artifact = SessionArtifact {
                    id: SessionArtifactId::new(),
                    session_id,
                    kind,
                    reference,
                    metadata: metadata.unwrap_or(serde_json::Value::Null),
                    created_at: time::now(),
                };
                Ok(vec![Event::SessionArtifactAttached { artifact }])
            }

            // ── Run signal commands — Linear B.5 ─────────────────────────────
            Command::SendRunSignal { run_id, kind } => {
                self.runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                let event = match kind {
                    SignalKind::Stop { reason } => Event::RunStopRequested {
                        run_id,
                        reason,
                        by: actor.clone(),
                    },
                    SignalKind::Elicit { prompt, choices } => Event::RunElicitationRequested {
                        run_id,
                        prompt,
                        choices,
                    },
                    SignalKind::AuthRequired { scope } => Event::RunAuthRequired { run_id, scope },
                    SignalKind::InterventionAccepted { .. } => {
                        return Err(CoreError::validation(
                            "use RespondRunSignal to respond to an elicitation",
                        ));
                    }
                };
                Ok(vec![event])
            }

            Command::RespondRunSignal { run_id, choice } => {
                self.runs
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("run repository not configured"))?
                    .get(run_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("run {run_id}")))?;

                Ok(vec![Event::RunInterventionAccepted {
                    run_id,
                    choice,
                    by: actor.clone(),
                }])
            }

            // ── Relation commands (§3.2 W2.1) ────────────────────────────────
            Command::LinkTasks { from, to, kind } => {
                let relations = self
                    .relations
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("relation repository not configured"))?;

                // Cycle detection (only for Blocks kind; DFS up to MAX_DEPTH).
                relation_enforcement::detect_cycle(relations, from, to, kind).await?;

                let now = time::now();
                let relation = Relation {
                    id: RelationId::new(),
                    from,
                    to,
                    kind,
                    created_at: now,
                    created_by: actor.clone(),
                };
                let relation_id = relation.id;

                // Insert — UNIQUE violation is already mapped to CoreError::Conflict
                // by RelationRepo::insert; just propagate.
                relations.insert(&relation).await?;

                Ok(vec![Event::TaskLinked {
                    relation_id,
                    from,
                    to,
                    kind,
                    actor: actor.clone(),
                    occurred_at: now,
                }])
            }

            Command::UnlinkTasks { id } => {
                let relations = self
                    .relations
                    .as_ref()
                    .ok_or_else(|| CoreError::storage("relation repository not configured"))?;

                // Load relation to populate event fields.
                let relation = relations
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("relation {id}")))?;

                let now = time::now();
                let from = relation.from;
                let to = relation.to;
                let kind = relation.kind;

                relations.delete(id).await?;

                Ok(vec![Event::TaskUnlinked {
                    relation_id: id,
                    from,
                    to,
                    kind,
                    occurred_at: now,
                }])
            }

            // ── Claim commands ────────────────────────────────────────────────
            Command::AcquireClaim {
                agent_id,
                task_id,
                ttl_secs,
            } => {
                let now = time::now();
                let expires_at = now + chrono::Duration::seconds(ttl_secs as i64);
                Ok(vec![Event::AgentClaimed {
                    agent_id,
                    task_id,
                    expires_at,
                }])
            }

            Command::ReleaseClaim { agent_id, task_id } => {
                Ok(vec![Event::AgentReleased { agent_id, task_id }])
            }

            // ── Work-lease commands ───────────────────────────────────────────
            // The atomic reservation already happened in the repo; these commands
            // project it into the event log for audit + WS sync (idempotent).
            Command::ReserveFiles { leases } => Ok(vec![Event::FilesReserved { leases }]),

            Command::ReleaseFiles { agent_id, task_id } => {
                Ok(vec![Event::FilesReleased { agent_id, task_id }])
            }

            // ── Handoff contracts (P5) ─────────────────────────────────────────
            Command::RequestHandoff { handoff } => {
                let repo = self
                    .handoffs
                    .as_ref()
                    .ok_or_else(|| CoreError::validation("handoff repository not configured"))?;
                if handoff.from_work_unit_id == handoff.to_work_unit_id {
                    return Err(CoreError::validation(
                        "handoff must connect two distinct work units",
                    ));
                }
                // Both endpoints must exist (same NotFound the drain uses).
                self.work_unit(handoff.from_work_unit_id).await?;
                self.work_unit(handoff.to_work_unit_id).await?;

                // One live contract per (from, to): re-requesting reuses the
                // existing id (reopens it); an accepted contract is settled
                // history and cannot be silently reopened.
                let id = match repo
                    .get_by_pair(handoff.from_work_unit_id, handoff.to_work_unit_id)
                    .await?
                {
                    Some(existing) if existing.status == daruma_domain::HandoffStatus::Accepted => {
                        return Err(CoreError::conflict(format!(
                            "handoff {} for this pair is already accepted",
                            existing.id
                        )));
                    }
                    Some(existing) => existing.id,
                    None => daruma_shared::HandoffId::new(),
                };
                Ok(vec![Event::HandoffRequested {
                    handoff: handoff.into_contract(id, time::now()),
                }])
            }

            Command::AcceptHandoff { handoff_id, notes } => {
                let repo = self
                    .handoffs
                    .as_ref()
                    .ok_or_else(|| CoreError::validation("handoff repository not configured"))?;
                let contract = repo
                    .get(handoff_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("handoff {handoff_id}")))?;
                match contract.status {
                    daruma_domain::HandoffStatus::Accepted => Ok(vec![]), // no-op
                    daruma_domain::HandoffStatus::Open => {
                        let by = match actor {
                            Actor::Agent { id, .. } => Some(*id),
                            _ => None,
                        };
                        Ok(vec![Event::HandoffAccepted {
                            handoff_id,
                            by,
                            notes,
                            at: time::now(),
                        }])
                    }
                    other => Err(CoreError::conflict(format!(
                        "handoff {handoff_id} is {}; only an open handoff can be accepted \
                         (re-request after rejection)",
                        other.as_str()
                    ))),
                }
            }

            Command::RejectHandoff {
                handoff_id,
                reason,
                required_changes,
            } => {
                let repo = self
                    .handoffs
                    .as_ref()
                    .ok_or_else(|| CoreError::validation("handoff repository not configured"))?;
                if reason.trim().is_empty() {
                    return Err(CoreError::validation("rejection reason must not be empty"));
                }
                let contract = repo
                    .get(handoff_id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("handoff {handoff_id}")))?;
                match contract.status {
                    daruma_domain::HandoffStatus::Rejected => Ok(vec![]), // no-op
                    daruma_domain::HandoffStatus::Open => Ok(vec![Event::HandoffRejected {
                        handoff_id,
                        reason,
                        required_changes,
                        at: time::now(),
                    }]),
                    other => Err(CoreError::conflict(format!(
                        "handoff {handoff_id} is {}; only an open handoff can be rejected",
                        other.as_str()
                    ))),
                }
            }

            // ── Document commands (PR1 §6.2) ──────────────────────────────────
            Command::CreateDocument { new_doc } => {
                let title = new_doc.title.trim().to_string();
                if title.is_empty() {
                    return Err(CoreError::validation("document title must not be empty"));
                }
                if title.len() > 500 {
                    return Err(CoreError::validation(
                        "document title must not exceed 500 characters",
                    ));
                }
                let id = new_doc.id.unwrap_or_else(DocumentId::new);
                let now = time::now();
                if let Some(task_id) = new_doc.task_id {
                    if self.tasks.get(task_id).await?.is_none() {
                        return Err(CoreError::not_found(format!("task {task_id}")));
                    }
                }
                let document = daruma_domain::NewDocument {
                    id: Some(id),
                    project_id: new_doc.project_id,
                    kind: new_doc.kind,
                    title,
                    content: new_doc.content,
                    status: new_doc.status,
                    task_id: new_doc.task_id,
                    trigger_kind: new_doc.trigger_kind,
                    consumer: new_doc.consumer,
                }
                .into_document(id, now);
                Ok(vec![Event::DocumentCreated { document }])
            }

            Command::ReplaceDocumentContent {
                document_id,
                content,
            } => {
                require_document(&self.documents, document_id).await?;
                Ok(vec![Event::DocumentContentReplaced {
                    document_id,
                    content,
                    at: time::now(),
                }])
            }

            Command::AppendDocumentContent {
                document_id,
                append,
            } => {
                require_document(&self.documents, document_id).await?;
                Ok(vec![Event::DocumentContentAppended {
                    document_id,
                    append,
                    at: time::now(),
                }])
            }

            Command::RenameDocument { document_id, title } => {
                let trimmed = title.trim().to_string();
                if trimmed.is_empty() {
                    return Err(CoreError::validation("document title must not be empty"));
                }
                if trimmed.len() > 500 {
                    return Err(CoreError::validation(
                        "document title must not exceed 500 characters",
                    ));
                }
                require_document(&self.documents, document_id).await?;
                Ok(vec![Event::DocumentRenamed {
                    document_id,
                    title: trimmed,
                    at: time::now(),
                }])
            }

            Command::ArchiveDocument { document_id } => {
                let doc = require_document(&self.documents, document_id).await?;
                if doc.archived_at.is_some() {
                    return Ok(vec![]); // already archived — no-op
                }
                Ok(vec![Event::DocumentArchived {
                    document_id,
                    at: time::now(),
                }])
            }

            Command::SetDocumentStatus {
                document_id,
                status,
            } => {
                let doc = require_document(&self.documents, document_id).await?;
                if doc.status == status {
                    return Ok(vec![]); // no-op — same status
                }
                Ok(vec![Event::DocumentStatusChanged {
                    document_id,
                    from: doc.status,
                    to: status,
                    at: time::now(),
                }])
            }

            Command::LinkDocumentToTask {
                document_id,
                task_id,
            } => {
                let doc = require_document(&self.documents, document_id).await?;
                if let Some(task_id) = task_id {
                    if self.tasks.get(task_id).await?.is_none() {
                        return Err(CoreError::not_found(format!("task {task_id}")));
                    }
                }
                if doc.task_id == task_id {
                    return Ok(vec![]); // no-op — link unchanged
                }
                Ok(vec![Event::DocumentTaskLinkChanged {
                    document_id,
                    task_id,
                    at: time::now(),
                }])
            }

            // ── Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4) ──────────────
            Command::CreateRule { rule } => {
                let repo = require_rules(&self.rules)?;
                if rule.rule_key.trim().is_empty() {
                    return Err(CoreError::validation("rule_key must not be empty"));
                }
                // Spec §2: one rule_key per scope level (also enforced by the
                // unique index; checked here for a clean error).
                if repo
                    .list_for_scope(&rule.scope)
                    .await?
                    .iter()
                    .any(|r| r.rule_key == rule.rule_key)
                {
                    return Err(CoreError::conflict(format!(
                        "rule_key `{}` already exists at this scope",
                        rule.rule_key
                    )));
                }
                let mut rule = rule.into_rule(time::now());
                if rule.id == daruma_shared::RuleId::default() {
                    rule.id = daruma_shared::RuleId::new();
                }
                Ok(vec![Event::RuleCreated { rule }])
            }

            Command::UpdateRule { id, patch } => {
                let repo = require_rules(&self.rules)?;
                if patch.is_empty() {
                    return Err(CoreError::validation("update must set at least one field"));
                }
                let current = repo
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("rule {id}")))?;
                let updated = patch.apply(current, time::now());
                Ok(vec![Event::RuleUpdated { rule: updated }])
            }

            Command::DisableRule { id } => {
                let repo = require_rules(&self.rules)?;
                let current = repo
                    .get(id)
                    .await?
                    .ok_or_else(|| CoreError::not_found(format!("rule {id}")))?;
                if !current.enabled {
                    return Ok(vec![]); // already disabled — no-op
                }
                Ok(vec![Event::RuleDisabled {
                    rule_id: id,
                    at: time::now(),
                }])
            }

            // ── Evidence registry (OSS task 019eb65a-3185; spec §1.3) ──────────
            Command::RecordEvidence { evidence } => {
                let _repo = require_evidence(&self.evidence)?;
                let supersedes = evidence.supersedes;
                let mut record = evidence.into_evidence(ActorRef::from_actor(actor), time::now());
                if record.id == daruma_shared::EvidenceId::default() {
                    record.id = daruma_shared::EvidenceId::new();
                }
                let new_id = record.id;
                let mut events = vec![Event::EvidenceRecorded { evidence: record }];
                // Immutability: a correction marks the old record, never edits.
                if let Some(old) = supersedes {
                    events.push(Event::EvidenceSuperseded {
                        evidence_id: old,
                        superseded_by: new_id,
                        at: time::now(),
                    });
                }
                Ok(events)
            }
        }
    }
}

/// Borrow the rule repo or return a clean "not configured" error. Every rule
/// CRUD command needs it; centralised so the message stays consistent.
fn require_rules(rules: &Option<Arc<dyn RuleRepository>>) -> Result<&Arc<dyn RuleRepository>> {
    rules
        .as_ref()
        .ok_or_else(|| CoreError::storage("rule repository not configured"))
}

/// Build one `RuleFired` audit event per acting rule from a gate decision.
///
/// `outcomes` yields `(details, message)` for each rule that warned or blocked;
/// `details` is the per-rule JSON the gate already assembled (`rule_id`,
/// `rule_key`). A rule whose `details` carries no parseable `rule_id` is
/// skipped — the audit log records identified rules, never guesses.
fn rule_fired_events<'a>(
    check: &GateCheck,
    actor: &Actor,
    decision: EventRuleDecision,
    outcomes: impl Iterator<Item = (&'a serde_json::Value, &'a str)>,
) -> Vec<Event> {
    let now = time::now();
    let trigger = check.trigger.as_str().to_string();
    outcomes
        .filter_map(|(details, message)| {
            let rule_id: daruma_shared::RuleId = details
                .get("rule_id")
                .and_then(|v| v.as_str())?
                .parse()
                .ok()?;
            let rule_key = details
                .get("rule_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(Event::RuleFired {
                rule_id,
                rule_key,
                decision,
                trigger: trigger.clone(),
                actor: actor.clone(),
                project_id: check.project_id,
                plan_id: check.plan_id,
                task_id: check.task_id,
                message: message.to_string(),
                at: now,
            })
        })
        .collect()
}

/// Flatten a `GateDecision::Blocked` `details` into `(per-rule details, message)`
/// pairs for [`rule_fired_events`]. The bundled `RuleEngine` packs every blocked
/// rule into `details.outcomes`; the fallback to the top-level `details` keeps a
/// single `RuleFired` for any other `LifecycleGate` impl whose `Blocked` payload
/// has no structured outcomes (e.g. a custom gate, exercised by the tests).
fn blocked_outcomes(
    details: &serde_json::Value,
    message: &str,
) -> Vec<(serde_json::Value, String)> {
    if let Some(outcomes) = details.get("outcomes").and_then(|v| v.as_array()) {
        let blocked: Vec<_> = outcomes
            .iter()
            .filter(|o| o.get("decision").and_then(|d| d.as_str()) == Some("blocked"))
            .map(|o| {
                let msg = o
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(message)
                    .to_string();
                (o.clone(), msg)
            })
            .collect();
        if !blocked.is_empty() {
            return blocked;
        }
    }
    vec![(details.clone(), message.to_string())]
}

/// Borrow the evidence repo or return a clean "not configured" error.
fn require_evidence(
    evidence: &Option<Arc<dyn EvidenceRepository>>,
) -> Result<&Arc<dyn EvidenceRepository>> {
    evidence
        .as_ref()
        .ok_or_else(|| CoreError::storage("evidence repository not configured"))
}

/// Fetch the document or return `not_found`. Used by every Document-mutation
/// command except `CreateDocument`. Centralised so the "no repo wired" and
/// "no such id" error messages stay consistent.
async fn require_document(
    documents: &Option<Arc<dyn DocumentRepository>>,
    id: DocumentId,
) -> Result<daruma_domain::Document> {
    let repo = documents
        .as_ref()
        .ok_or_else(|| CoreError::storage("document repository not configured"))?;
    repo.get(id)
        .await?
        .ok_or_else(|| CoreError::not_found(format!("document {id}")))
}

// ── bulk-op helpers (§3.7.7 / LIN B.7) ────────────────────────────────────────

/// Hard cap on the number of ids accepted by a single bulk command.
const BULK_OP_CAP: usize = 50;

/// §3.8.2 — maximum byte length of a `RunNoteAppended.body` payload.
/// Free-form narrative, not full markdown documents; 4 KiB keeps any single
/// note small enough to embed inline in WS / inbox frames.
const RUN_NOTE_MAX_BYTES: usize = 4096;

/// Validate that a deduped bulk-op id list is non-empty and within the cap.
fn validate_bulk_cap(n: usize) -> Result<()> {
    if n == 0 {
        return Err(CoreError::validation("bulk op requires at least 1 id"));
    }
    if n > BULK_OP_CAP {
        return Err(CoreError::validation(format!(
            "bulk size {n} exceeds cap of {BULK_OP_CAP}"
        )));
    }
    Ok(())
}

/// Deduplicate `TaskId`s while preserving first-seen order.
fn dedupe_ids(ids: &[TaskId]) -> Vec<TaskId> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use daruma_domain::{
        Actor, AgentSession, NewTask, Plan, PlanStatus as PS, PlanTask, Run, SessionArtifactKind,
    };
    use daruma_events::{Event, EventStore};
    use daruma_shared::{ProjectId, RunId, TaskId};
    use daruma_storage::{ActivityRepo, CommentRepo, Db, ProjectRepo, SqliteEventStore, TaskRepo};
    use std::{collections::HashMap, sync::Mutex};

    use crate::{
        repos::{ExternalRefRepository, PlanRepository, RunRepository, SessionRepository},
        search::{SearchHit, SearchIndexItem, SearchProvider, SearchQuery},
    };

    // ── Original task/project test stack (unchanged) ──────────────────────────

    async fn build_stack() -> (CommandHandler, Arc<dyn EventStore>, Arc<TaskRepo>) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool));
        let bus = EventBus::default();
        let handler = CommandHandler::new(
            store.clone(),
            tasks.clone(),
            projects.clone(),
            comments,
            activity,
            bus,
        );
        (handler, store, tasks)
    }

    // ── In-memory stub repos ──────────────────────────────────────────────────

    #[derive(Default)]
    struct MemPlanRepo {
        plans: Mutex<HashMap<PlanId, Plan>>,
        plan_tasks: Mutex<HashMap<PlanId, Vec<PlanTask>>>,
    }

    #[async_trait]
    impl PlanRepository for MemPlanRepo {
        async fn get(&self, id: PlanId) -> daruma_shared::Result<Option<Plan>> {
            Ok(self.plans.lock().unwrap().get(&id).cloned())
        }

        async fn list_plan_tasks_ordered(
            &self,
            plan_id: PlanId,
        ) -> daruma_shared::Result<Vec<PlanTask>> {
            let mut v = self
                .plan_tasks
                .lock()
                .unwrap()
                .get(&plan_id)
                .cloned()
                .unwrap_or_default();
            v.sort_by_key(|t| t.position);
            Ok(v)
        }

        async fn list_plans_for_task(&self, task_id: TaskId) -> daruma_shared::Result<Vec<PlanId>> {
            let guard = self.plan_tasks.lock().unwrap();
            let plan_ids = guard
                .iter()
                .filter_map(|(plan_id, tasks)| {
                    if tasks.iter().any(|t| t.task_id == task_id) {
                        Some(*plan_id)
                    } else {
                        None
                    }
                })
                .collect();
            Ok(plan_ids)
        }

        async fn apply_event(&self, env: &EventEnvelope) -> daruma_shared::Result<()> {
            match &env.payload {
                Event::PlanCreated { plan } => {
                    self.plans.lock().unwrap().insert(plan.id, plan.clone());
                }
                Event::PlanStatusChanged { plan_id, to, .. } => {
                    if let Some(p) = self.plans.lock().unwrap().get_mut(plan_id) {
                        p.status = *to;
                    }
                }
                Event::PlanArchived { plan_id, at } => {
                    if let Some(p) = self.plans.lock().unwrap().get_mut(plan_id) {
                        p.archived_at = Some(*at);
                        p.status = PS::Abandoned;
                    }
                }
                Event::PlanTaskAdded {
                    plan_id,
                    task_id,
                    position,
                    depends_on,
                } => {
                    self.plan_tasks
                        .lock()
                        .unwrap()
                        .entry(*plan_id)
                        .or_default()
                        .push(PlanTask {
                            plan_id: *plan_id,
                            task_id: *task_id,
                            position: *position,
                            depends_on: depends_on.clone(),
                        });
                }
                Event::PlanTaskRemoved { plan_id, task_id } => {
                    if let Some(tasks) = self.plan_tasks.lock().unwrap().get_mut(plan_id) {
                        tasks.retain(|t| t.task_id != *task_id);
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemRunRepo {
        runs: Mutex<HashMap<RunId, Run>>,
        current_steps: Mutex<HashMap<RunId, TaskId>>,
    }

    #[async_trait]
    impl RunRepository for MemRunRepo {
        async fn get(&self, id: RunId) -> daruma_shared::Result<Option<Run>> {
            Ok(self.runs.lock().unwrap().get(&id).cloned())
        }

        async fn list_active_for_plan(&self, plan_id: PlanId) -> daruma_shared::Result<Vec<Run>> {
            Ok(self
                .runs
                .lock()
                .unwrap()
                .values()
                .filter(|r| r.plan_id == plan_id && r.status == RunStatus::Active)
                .cloned()
                .collect())
        }

        async fn current_step_task(&self, run_id: RunId) -> daruma_shared::Result<Option<TaskId>> {
            Ok(self.current_steps.lock().unwrap().get(&run_id).copied())
        }

        async fn list_unresponsive_candidates(
            &self,
            _threshold: std::time::Duration,
            _now: daruma_shared::Timestamp,
        ) -> daruma_shared::Result<Vec<RunId>> {
            Ok(vec![])
        }

        async fn list_stale_candidates(
            &self,
            _threshold: std::time::Duration,
            _now: daruma_shared::Timestamp,
        ) -> daruma_shared::Result<Vec<RunId>> {
            Ok(vec![])
        }

        async fn apply_event(&self, env: &EventEnvelope) -> daruma_shared::Result<()> {
            match &env.payload {
                Event::RunStarted { run } => {
                    self.runs.lock().unwrap().insert(run.id, run.clone());
                }
                Event::RunStepStarted {
                    run_id, task_id, ..
                } => {
                    self.current_steps.lock().unwrap().insert(*run_id, *task_id);
                }
                Event::RunStepFinished { run_id, .. } => {
                    self.current_steps.lock().unwrap().remove(run_id);
                }
                Event::RunCompleted { run_id, at } => {
                    if let Some(r) = self.runs.lock().unwrap().get_mut(run_id) {
                        r.status = RunStatus::Completed;
                        r.ended_at = Some(*at);
                    }
                }
                Event::RunFailed { run_id, reason, at } => {
                    if let Some(r) = self.runs.lock().unwrap().get_mut(run_id) {
                        r.status = RunStatus::Failed;
                        r.ended_at = Some(*at);
                        r.outcome = Some(reason.clone());
                    }
                }
                Event::RunAborted { run_id, reason, at } => {
                    if let Some(r) = self.runs.lock().unwrap().get_mut(run_id) {
                        r.status = RunStatus::Aborted;
                        r.ended_at = Some(*at);
                        r.outcome = Some(reason.clone());
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemSessionRepo {
        sessions: Mutex<HashMap<AgentSessionId, AgentSession>>,
    }

    #[async_trait]
    impl SessionRepository for MemSessionRepo {
        async fn get(&self, id: AgentSessionId) -> daruma_shared::Result<Option<AgentSession>> {
            Ok(self.sessions.lock().unwrap().get(&id).cloned())
        }

        async fn apply_event(&self, env: &EventEnvelope) -> daruma_shared::Result<()> {
            match &env.payload {
                Event::AgentSessionStarted { session } => {
                    self.sessions
                        .lock()
                        .unwrap()
                        .insert(session.id, session.clone());
                }
                Event::AgentSessionEnded { session_id, at } => {
                    if let Some(s) = self.sessions.lock().unwrap().get_mut(session_id) {
                        s.ended_at = Some(*at);
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemExtRefRepo {
        refs: Mutex<HashMap<(String, String, String), String>>,
    }

    impl MemExtRefRepo {
        fn seed(&self, tenant: &str, kind: &str, ext_id: &str, internal_id: &str) {
            self.refs.lock().unwrap().insert(
                (tenant.into(), kind.into(), ext_id.into()),
                internal_id.into(),
            );
        }
    }

    #[async_trait]
    impl ExternalRefRepository for MemExtRefRepo {
        async fn lookup(
            &self,
            tenant: &str,
            kind: &str,
            external_id: &str,
        ) -> daruma_shared::Result<Option<String>> {
            Ok(self
                .refs
                .lock()
                .unwrap()
                .get(&(tenant.into(), kind.into(), external_id.into()))
                .cloned())
        }

        async fn apply_event(&self, _env: &EventEnvelope) -> daruma_shared::Result<()> {
            Ok(())
        }
    }

    struct RecordingSearchProvider {
        tx: tokio::sync::mpsc::UnboundedSender<String>,
    }

    #[async_trait]
    impl SearchProvider for RecordingSearchProvider {
        async fn search(&self, _query: SearchQuery) -> daruma_shared::Result<Vec<SearchHit>> {
            Ok(Vec::new())
        }

        async fn index(&self, item: SearchIndexItem) -> daruma_shared::Result<()> {
            if let SearchIndexItem::Task(task) = item {
                let _ = self.tx.send(format!("task:{}", task.title));
            }
            Ok(())
        }
    }

    /// Build a full handler with all stub repos wired.
    async fn build_plan_stack() -> (
        CommandHandler,
        Arc<MemPlanRepo>,
        Arc<MemRunRepo>,
        Arc<MemSessionRepo>,
        Arc<MemExtRefRepo>,
        Arc<TaskRepo>,
    ) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(pool.clone()));
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let projects = Arc::new(ProjectRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let activity = Arc::new(ActivityRepo::new(pool));
        let bus = EventBus::default();

        let plans = Arc::new(MemPlanRepo::default());
        let runs = Arc::new(MemRunRepo::default());
        let sessions = Arc::new(MemSessionRepo::default());
        let ext_refs = Arc::new(MemExtRefRepo::default());

        let handler = CommandHandler::new(store, tasks.clone(), projects, comments, activity, bus)
            .with_plans(plans.clone() as Arc<dyn PlanRepository>)
            .with_runs(runs.clone() as Arc<dyn RunRepository>)
            .with_sessions(sessions.clone() as Arc<dyn SessionRepository>)
            .with_external_refs(ext_refs.clone() as Arc<dyn ExternalRefRepository>);

        (handler, plans, runs, sessions, ext_refs, tasks)
    }

    // ── Original task / project tests (unchanged) ─────────────────────────────

    #[tokio::test]
    async fn create_task_produces_event_and_projection() {
        let (handler, store, tasks) = build_stack().await;

        let envs = handler
            .handle(
                Command::CreateTask {
                    task: NewTask::new("Integration test"),
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(envs.len(), 1);
        assert!(envs[0].seq > 0);

        let events = store.load_since(0, 100).await.unwrap();
        assert_eq!(events.len(), 1);

        let all = tasks.list_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Integration test");
    }

    #[tokio::test]
    async fn create_task_updates_search_index_asynchronously() {
        let (handler, _store, _tasks) = build_stack().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handler = handler.with_search_provider(Arc::new(RecordingSearchProvider { tx }));

        handler
            .handle(
                Command::CreateTask {
                    task: NewTask::new("Indexed task"),
                },
                Actor::user(),
            )
            .await
            .unwrap();

        let indexed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(indexed, "task:Indexed task");
    }

    #[tokio::test]
    async fn complete_task_emits_three_events_and_conflicts_on_repeat() {
        let (handler, store, tasks) = build_stack().await;

        let task_id = {
            let envs = handler
                .handle(
                    Command::CreateTask {
                        task: NewTask::new("Complete me"),
                    },
                    Actor::user(),
                )
                .await
                .unwrap();
            match &envs[0].payload {
                Event::TaskCreated { task } => task.id.unwrap(),
                _ => panic!("expected TaskCreated"),
            }
        };

        let complete_envs = handler
            .handle(
                Command::CompleteTask {
                    id: task_id,
                    note: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        assert_eq!(complete_envs.len(), 3);

        assert_eq!(store.load_since(0, 100).await.unwrap().len(), 4);

        let task = tasks.get(task_id).await.unwrap().unwrap();
        assert_eq!(task.status, daruma_domain::Status::Done);
        assert!(task.completed_at.is_some());

        let err = handler
            .handle(
                Command::CompleteTask {
                    id: task_id,
                    note: None,
                },
                Actor::user(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, daruma_shared::CoreError::Conflict(_)));
    }

    #[tokio::test]
    async fn split_task_creates_subtasks_in_projection() {
        let (handler, store, tasks) = build_stack().await;

        let parent_id = {
            let envs = handler
                .handle(
                    Command::CreateTask {
                        task: NewTask::new("Parent"),
                    },
                    Actor::user(),
                )
                .await
                .unwrap();
            match &envs[0].payload {
                Event::TaskCreated { task } => task.id.unwrap(),
                _ => panic!("expected TaskCreated"),
            }
        };

        let split_envs = handler
            .handle(
                Command::SplitTask {
                    parent: parent_id,
                    subtasks: vec![NewTask::new("Sub A"), NewTask::new("Sub B")],
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(split_envs.len(), 3);
        assert_eq!(store.load_since(0, 100).await.unwrap().len(), 4);
        assert_eq!(tasks.list_all().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn add_comment_emits_comment_added() {
        use daruma_domain::NewComment;

        let (handler, _store, _tasks) = build_stack().await;

        let task_id = TaskId::new();
        let envs = handler
            .handle(
                Command::AddComment {
                    comment: NewComment {
                        id: None,
                        task_id,
                        body: "looks good".to_string(),
                        parent_id: None,
                        kind: None,
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(envs.len(), 2);
        assert!(
            matches!(&envs[0].payload, Event::CommentAdded { comment } if comment.task_id == task_id)
        );
        assert!(
            matches!(&envs[1].payload, Event::TaskCommented { task_id: t, .. } if *t == task_id)
        );
    }

    #[tokio::test]
    async fn add_comment_rejects_empty_body() {
        use daruma_domain::NewComment;

        let (handler, _store, _tasks) = build_stack().await;

        let err = handler
            .handle(
                Command::AddComment {
                    comment: NewComment {
                        id: None,
                        task_id: TaskId::new(),
                        body: "   ".to_string(),
                        parent_id: None,
                        kind: None,
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn edit_deleted_comment_returns_not_found() {
        use daruma_domain::{CommentPatch, NewComment};

        let (handler, _store, _tasks) = build_stack().await;

        let task_id = TaskId::new();

        let envs = handler
            .handle(
                Command::AddComment {
                    comment: NewComment {
                        id: None,
                        task_id,
                        body: "original".to_string(),
                        parent_id: None,
                        kind: None,
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let comment_id = match &envs[0].payload {
            Event::CommentAdded { comment } => comment.id,
            _ => panic!("expected CommentAdded"),
        };

        handler
            .handle(Command::DeleteComment { id: comment_id }, Actor::user())
            .await
            .unwrap();

        let err = handler
            .handle(
                Command::EditComment {
                    id: comment_id,
                    patch: CommentPatch {
                        body: Some("new body".to_string()),
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, CoreError::NotFound(_)));
    }

    // ── Plan / Run / Session / Claim tests (W2.2) ─────────────────────────────

    /// Helper: create a plan and return its id.
    async fn create_active_plan(handler: &CommandHandler, project_id: ProjectId) -> PlanId {
        use daruma_domain::NewPlan;

        let envs = handler
            .handle(
                Command::CreatePlan {
                    plan: NewPlan::new("Test plan", project_id, Actor::user()),
                    external_ref: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let plan_id = match &envs[0].payload {
            Event::PlanCreated { plan } => plan.id,
            _ => panic!("expected PlanCreated"),
        };

        handler
            .handle(
                Command::SetPlanStatus {
                    plan_id,
                    status: PS::Active,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        plan_id
    }

    #[tokio::test]
    async fn create_plan_happy_path_emits_plan_created() {
        use daruma_domain::NewPlan;

        let (handler, plans, ..) = build_plan_stack().await;
        let project_id = ProjectId::new();

        let envs = handler
            .handle(
                Command::CreatePlan {
                    plan: NewPlan::new("My plan", project_id, Actor::user()),
                    external_ref: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(envs.len(), 1);
        let plan_id = match &envs[0].payload {
            Event::PlanCreated { plan } => {
                assert_eq!(plan.title, "My plan");
                assert_eq!(plan.project_id, project_id);
                plan.id
            }
            _ => panic!("expected PlanCreated"),
        };

        // Projection was updated via apply_event
        let stored = plans.get(plan_id).await.unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().title, "My plan");
    }

    #[tokio::test]
    async fn create_plan_rejects_empty_title() {
        use daruma_domain::NewPlan;

        let (handler, ..) = build_plan_stack().await;
        let err = handler
            .handle(
                Command::CreatePlan {
                    plan: NewPlan::new("   ", ProjectId::new(), Actor::user()),
                    external_ref: None,
                },
                Actor::user(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn create_plan_with_existing_external_ref_is_idempotent() {
        use daruma_domain::NewPlan;

        let (handler, _, _, _, ext_refs, _) = build_plan_stack().await;
        ext_refs.seed(
            "omc",
            "plan",
            "plan-abc-001",
            "pln_00000000-0000-0000-0000-000000000001",
        );

        let envs = handler
            .handle(
                Command::CreatePlan {
                    plan: NewPlan::new("Duplicate", ProjectId::new(), Actor::user()),
                    external_ref: Some(("omc".into(), "plan".into(), "plan-abc-001".into())),
                },
                Actor::user(),
            )
            .await
            .unwrap();

        // No events emitted — idempotent no-op
        assert!(envs.is_empty());
    }

    #[tokio::test]
    async fn remove_plan_task_current_step_emits_three_events() {
        let (handler, _, runs, _, _, _tasks_repo) = build_plan_stack().await;
        let project_id = ProjectId::new();

        // Create a task in SQLite so task existence check passes
        let task_envs = handler
            .handle(
                Command::CreateTask {
                    task: NewTask::new("Step task"),
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let task_id = match &task_envs[0].payload {
            Event::TaskCreated { task } => task.id.unwrap(),
            _ => panic!(),
        };

        let plan_id = create_active_plan(&handler, project_id).await;

        // Attach task to plan
        handler
            .handle(
                Command::AddPlanTask {
                    plan_id,
                    task_id,
                    position: Some(0),
                    depends_on: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        // Start a run
        let agent_id = AgentId::new();
        let run_envs = handler
            .handle(
                Command::StartRun {
                    plan_id,
                    agent_id,
                    parent_run_id: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let run_id = match &run_envs[0].payload {
            Event::RunStarted { run } => run.id,
            _ => panic!(),
        };

        // Mark the task as the current step
        handler
            .handle(
                Command::RunStartStep { run_id, task_id },
                Actor::agent("agent-1"),
            )
            .await
            .unwrap();

        // Verify stub state
        assert_eq!(runs.current_step_task(run_id).await.unwrap(), Some(task_id));

        // RemovePlanTask — should detect contest and emit 3 events
        let remove_envs = handler
            .handle(Command::RemovePlanTask { plan_id, task_id }, Actor::user())
            .await
            .unwrap();

        assert_eq!(
            remove_envs.len(),
            3,
            "expected PlanTaskRemoved + TaskContested + RunStepFinished"
        );

        assert!(matches!(
            &remove_envs[0].payload,
            Event::PlanTaskRemoved { task_id: t, .. } if *t == task_id
        ));
        assert!(matches!(
            &remove_envs[1].payload,
            Event::TaskContested { task_id: t, .. } if *t == task_id
        ));
        assert!(matches!(
            &remove_envs[2].payload,
            Event::RunStepFinished {
                outcome: RunOutcome::Superseded,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn archive_plan_with_active_run_emits_three_events() {
        let (handler, _, _runs, ..) = build_plan_stack().await;
        let project_id = ProjectId::new();
        let plan_id = create_active_plan(&handler, project_id).await;

        // Start a run
        handler
            .handle(
                Command::StartRun {
                    plan_id,
                    agent_id: AgentId::new(),
                    parent_run_id: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        // Archive plan — should abort the run atomically
        let envs = handler
            .handle(Command::ArchivePlan { id: plan_id }, Actor::user())
            .await
            .unwrap();

        // PlanArchived + RunAborted + RunObsolescedByPlanEdit = 3
        assert_eq!(envs.len(), 3);
        assert!(matches!(&envs[0].payload, Event::PlanArchived { .. }));
        assert!(
            matches!(&envs[1].payload, Event::RunAborted { reason, .. } if reason == "plan_archived")
        );
        assert!(matches!(
            &envs[2].payload,
            Event::RunObsolescedByPlanEdit {
                kind: ObsolescenceKind::Archived,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn update_agent_session_plan_rejects_over_100_steps() {
        use daruma_domain::AgentSessionPlanStep;
        use daruma_domain::SessionStepStatus;

        let (handler, _, _, _sessions, ..) = build_plan_stack().await;

        // Start a session
        let sess_envs = handler
            .handle(
                Command::StartAgentSession {
                    agent_id: AgentId::new(),
                    parent_agent_id: None,
                    metadata: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let session_id = match &sess_envs[0].payload {
            Event::AgentSessionStarted { session } => session.id,
            _ => panic!(),
        };

        // 101 steps → validation error
        let steps: Vec<AgentSessionPlanStep> = (0..=100)
            .map(|i| AgentSessionPlanStep {
                content: format!("step {i}"),
                status: SessionStepStatus::Pending,
            })
            .collect();
        assert_eq!(steps.len(), 101);

        let err = handler
            .handle(
                Command::UpdateAgentSessionPlan {
                    id: session_id,
                    steps,
                },
                Actor::user(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[tokio::test]
    async fn attach_session_artifact_emits_event() {
        let (handler, _, _, _sessions, ..) = build_plan_stack().await;

        let sess_envs = handler
            .handle(
                Command::StartAgentSession {
                    agent_id: AgentId::new(),
                    parent_agent_id: None,
                    metadata: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let session_id = match &sess_envs[0].payload {
            Event::AgentSessionStarted { session } => session.id,
            _ => panic!(),
        };

        let envs = handler
            .handle(
                Command::AttachSessionArtifact {
                    session_id,
                    kind: SessionArtifactKind::File,
                    reference: "target/report.txt".into(),
                    metadata: Some(serde_json::json!({"bytes": 42})),
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(envs.len(), 1);
        assert!(matches!(
            &envs[0].payload,
            Event::SessionArtifactAttached { artifact }
                if artifact.session_id == session_id
                    && artifact.kind == SessionArtifactKind::File
                    && artifact.reference == "target/report.txt"
                    && artifact.metadata["bytes"] == serde_json::json!(42)
        ));
    }

    #[tokio::test]
    async fn acquire_and_release_claim_emit_events() {
        let (handler, ..) = build_plan_stack().await;
        let agent_id = AgentId::new();
        let task_id = TaskId::new();

        let claim_envs = handler
            .handle(
                Command::AcquireClaim {
                    agent_id,
                    task_id,
                    ttl_secs: 60,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        assert_eq!(claim_envs.len(), 1);
        assert!(matches!(
            &claim_envs[0].payload,
            Event::AgentClaimed { agent_id: a, task_id: t, .. } if *a == agent_id && *t == task_id
        ));

        let release_envs = handler
            .handle(Command::ReleaseClaim { agent_id, task_id }, Actor::user())
            .await
            .unwrap();
        assert_eq!(release_envs.len(), 1);
        assert!(matches!(
            &release_envs[0].payload,
            Event::AgentReleased { agent_id: a, task_id: t } if *a == agent_id && *t == task_id
        ));
    }

    #[tokio::test]
    async fn send_run_signal_stop_emits_run_stop_requested() {
        let (handler, ..) = build_plan_stack().await;
        let plan_id = create_active_plan(&handler, ProjectId::new()).await;

        let run_envs = handler
            .handle(
                Command::StartRun {
                    plan_id,
                    agent_id: AgentId::new(),
                    parent_run_id: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let run_id = match &run_envs[0].payload {
            Event::RunStarted { run } => run.id,
            _ => panic!(),
        };

        let sig_envs = handler
            .handle(
                Command::SendRunSignal {
                    run_id,
                    kind: SignalKind::Stop {
                        reason: Some("user request".into()),
                    },
                },
                Actor::user(),
            )
            .await
            .unwrap();
        assert_eq!(sig_envs.len(), 1);
        assert!(matches!(
            &sig_envs[0].payload,
            Event::RunStopRequested { run_id: r, reason: Some(msg), .. }
            if *r == run_id && msg == "user request"
        ));
    }

    #[tokio::test]
    async fn set_plan_status_completed_blocked_by_unfinished_task() {
        let (handler, _, _, _, _, _tasks_repo) = build_plan_stack().await;
        let project_id = ProjectId::new();

        // Create a task (stays in default/non-terminal status)
        let task_envs = handler
            .handle(
                Command::CreateTask {
                    task: NewTask::new("Unfinished task"),
                },
                Actor::user(),
            )
            .await
            .unwrap();
        let task_id = match &task_envs[0].payload {
            Event::TaskCreated { task } => task.id.unwrap(),
            _ => panic!("expected TaskCreated"),
        };

        let plan_id = create_active_plan(&handler, project_id).await;

        handler
            .handle(
                Command::AddPlanTask {
                    plan_id,
                    task_id,
                    position: Some(0),
                    depends_on: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        let err = handler
            .handle(
                Command::SetPlanStatus {
                    plan_id,
                    status: PS::Completed,
                },
                Actor::user(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(err, daruma_shared::CoreError::Validation(_)),
            "expected Validation error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn set_plan_status_completed_allowed_when_all_tasks_terminal() {
        let (handler, _, _, _, _, _tasks_repo) = build_plan_stack().await;
        let project_id = ProjectId::new();

        // Create two tasks and move them to terminal states
        let mut task_ids = Vec::new();
        for title in ["Task A", "Task B"] {
            let envs = handler
                .handle(
                    Command::CreateTask {
                        task: NewTask::new(title),
                    },
                    Actor::user(),
                )
                .await
                .unwrap();
            let id = match &envs[0].payload {
                Event::TaskCreated { task } => task.id.unwrap(),
                _ => panic!("expected TaskCreated"),
            };
            task_ids.push(id);
        }

        // Complete task A (Done), cancel task B (Cancelled)
        handler
            .handle(
                Command::CompleteTask {
                    id: task_ids[0],
                    note: None,
                },
                Actor::user(),
            )
            .await
            .unwrap();
        handler
            .handle(
                Command::SetStatus {
                    id: task_ids[1],
                    status: daruma_domain::Status::Cancelled,
                    force: false,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        let plan_id = create_active_plan(&handler, project_id).await;

        for (pos, &task_id) in task_ids.iter().enumerate() {
            handler
                .handle(
                    Command::AddPlanTask {
                        plan_id,
                        task_id,
                        position: Some(pos as u32),
                        depends_on: None,
                    },
                    Actor::user(),
                )
                .await
                .unwrap();
        }

        let envs = handler
            .handle(
                Command::SetPlanStatus {
                    plan_id,
                    status: PS::Completed,
                },
                Actor::user(),
            )
            .await
            .unwrap();

        assert_eq!(envs.len(), 1);
        assert!(matches!(
            &envs[0].payload,
            Event::PlanStatusChanged { plan_id: p, to: PS::Completed, .. } if *p == plan_id
        ));
    }
}
