//! Fail-closed boundary for tokens restricted to projects. Global projections
//! are not project-scoped merely because the caller supplied a project query.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Method,
    response::Response,
};

fn denied() -> ApiError {
    ApiError::from(CoreError::forbidden(
        "this operation requires workspace-wide access",
    ))
}

pub(super) async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(auth) = req.extensions().get::<AuthContext>().cloned() else {
        return denied().into_response();
    };
    if matches!(auth.scope.projects, ProjectFilter::All) {
        return next.run(req).await;
    }
    match check(&state, &auth, req).await {
        Ok(req) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

async fn check(state: &AppState, auth: &AuthContext, req: Request) -> Result<Request, ApiError> {
    let path = req
        .uri()
        .path()
        .strip_prefix("/v1")
        .unwrap_or(req.uri().path())
        .to_owned();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    let read = req.method() == Method::GET;
    match parts.as_slice() {
        // Each collection handler filters before pagination. MCP re-enters REST.
        ["mcp"] => return Ok(req),
        ["tasks" | "projects" | "plans" | "search" | "repo-scopes"] if read => return Ok(req),
        ["tasks", id] if read => task(state, auth, parse_id(id.to_string(), "task")?).await?,
        ["tasks", id, "comments" | "activity" | "can_start" | "plans"] if read => {
            task(state, auth, parse_id(id.to_string(), "task")?).await?;
        }
        ["plans", id] if read => plan(state, auth, parse_id(id.to_string(), "plan")?).await?,
        ["plans", id, "progress" | "graph" | "fanout" | "next-task" | "runs"] if read => {
            plan(state, auth, parse_id(id.to_string(), "plan")?).await?;
        }
        ["projects", id, "settings" | "documents"] if read => {
            require_project_scope(auth, Some(parse_id(id.to_string(), "project")?))?
        }
        ["runs", id] | ["runs", id, "timeline" | "notes"] if read => {
            run(state, auth, parse_id(id.to_string(), "run")?).await?
        }
        ["documents", id] if read => {
            let doc = state
                .documents
                .get(parse_id(id.to_string(), "document")?)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(denied)?;
            require_project_scope(auth, Some(doc.project_id))?;
        }
        ["plans", id] if req.method() == Method::PATCH => {
            let id = parse_id(id.to_string(), "plan")?;
            let (req, body) = read_body::<UpdatePlanBody>(req).await?;
            command(
                state,
                auth,
                &Command::UpdatePlan {
                    id,
                    patch: body.patch,
                },
            )
            .await?;
            return Ok(req);
        }
        ["plans", id] if req.method() == Method::DELETE => {
            plan(state, auth, parse_id(id.to_string(), "plan")?).await?
        }
        ["plans", id, "tasks"] if req.method() == Method::POST => {
            let plan_id = parse_id(id.to_string(), "plan")?;
            let (req, b) = read_body::<AddPlanTaskBody>(req).await?;
            command(
                state,
                auth,
                &Command::AddPlanTask {
                    plan_id,
                    task_id: b.task_id,
                    position: b.position,
                    depends_on: b.depends_on,
                },
            )
            .await?;
            return Ok(req);
        }
        ["plans", id, "tasks", tid] if req.method() == Method::PATCH => {
            let plan_id = parse_id(id.to_string(), "plan")?;
            let task_id = parse_id(tid.to_string(), "task")?;
            let (req, b) = read_body::<AmendPlanTaskBody>(req).await?;
            command(
                state,
                auth,
                &Command::AmendPlanTask {
                    plan_id,
                    task_id,
                    patch: b.patch,
                },
            )
            .await?;
            return Ok(req);
        }
        ["plans", id, "tasks", tid] if req.method() == Method::DELETE => {
            plan(state, auth, parse_id(id.to_string(), "plan")?).await?;
            task(state, auth, parse_id(tid.to_string(), "task")?).await?;
        }
        ["plans", id, "reorder"] if req.method() == Method::POST => {
            let plan_id = parse_id(id.to_string(), "plan")?;
            let (req, b) = read_body::<ReorderPlanBody>(req).await?;
            command(
                state,
                auth,
                &Command::ReorderPlan {
                    plan_id,
                    order: b.order,
                },
            )
            .await?;
            return Ok(req);
        }
        ["comments", id] if req.method() == Method::PATCH || req.method() == Method::DELETE => {
            let comment = state
                .comments
                .get(parse_id(id.to_string(), "comment")?)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(denied)?;
            task(state, auth, comment.task_id).await?;
        }
        ["claims"] if read => return Ok(req), // The handler filters each claim's task.
        ["ready"] if read => {
            let Query(q) =
                Query::<ProjectReadyQuery>::try_from_uri(req.uri()).map_err(|_| denied())?;
            let project_id = parse_id(q.project_id, "project")?;
            require_project_scope(auth, Some(project_id))?;
            for p in state
                .plans
                .list_by_project(project_id, None)
                .await
                .map_err(ApiError::from)?
            {
                plan(state, auth, p.id).await?;
            }
        }
        ["plans", id, "drain-next"] if req.method() == Method::POST => {
            let id = parse_id(id.to_string(), "plan")?;
            plan(state, auth, id).await?;
            let (req, body) = read_body::<DrainNextBody>(req).await?;
            if let Some(id) = body.run_id {
                run(state, auth, id).await?;
            }
            return Ok(req);
        }
        ["plans", id, "status" | "archive"] if req.method() == Method::POST => {
            plan(state, auth, parse_id(id.to_string(), "plan")?).await?
        }
        ["claims", _, id] if req.method() == Method::DELETE => {
            task(state, auth, parse_id(id.to_string(), "task")?).await?
        }
        ["claims"] if req.method() == Method::POST => {
            let (req, body) = read_body::<AcquireClaimBody>(req).await?;
            task(state, auth, body.task_id).await?;
            return Ok(req);
        }
        ["tasks", id, "comments"] if req.method() == Method::POST => {
            let id = parse_id(id.to_string(), "task")?;
            task(state, auth, id).await?;
            let (req, body) = read_body::<AddCommentBody>(req).await?;
            for id in [body.id, body.parent_id].into_iter().flatten() {
                if let Some(comment) = state.comments.get(id).await.map_err(ApiError::from)? {
                    task(state, auth, comment.task_id).await?;
                }
            }
            return Ok(req);
        }
        ["runs"] if req.method() == Method::POST => {
            let (req, body) = read_body::<StartRunBody>(req).await?;
            plan(state, auth, body.plan_id).await?;
            if let Some(id) = body.parent_run_id {
                run(state, auth, id).await?;
            }
            return Ok(req);
        }
        ["runs", id, "complete" | "fail" | "abort" | "notes"] if req.method() == Method::POST => {
            run(state, auth, parse_id(id.to_string(), "run")?).await?
        }
        ["runs", id, "step", "start"] if req.method() == Method::POST => {
            run(state, auth, parse_id(id.to_string(), "run")?).await?;
            let (req, body) = read_body::<RunStartStepBody>(req).await?;
            task(state, auth, body.task_id).await?;
            return Ok(req);
        }
        ["runs", id, "step", "finish"] if req.method() == Method::POST => {
            run(state, auth, parse_id(id.to_string(), "run")?).await?;
            let (req, body) = read_body::<RunFinishStepBody>(req).await?;
            task(state, auth, body.task_id).await?;
            return Ok(req);
        }
        ["plans"] if req.method() == Method::POST => {
            let (req, body) = read_body::<CreatePlanBody>(req).await?;
            command(
                state,
                auth,
                &Command::CreatePlan {
                    plan: body.plan,
                    external_ref: body.external_ref,
                },
            )
            .await?;
            return Ok(req);
        }
        ["evidence"] if req.method() == Method::POST => {
            let (req, b) = read_body::<RecordEvidenceBody>(req).await?;
            command(
                state,
                auth,
                &Command::RecordEvidence {
                    evidence: b.evidence,
                },
            )
            .await?;
            return Ok(req);
        }
        ["evidence"] if read => {
            let Query(q) =
                Query::<RuleScopeQuery>::try_from_uri(req.uri()).map_err(|_| denied())?;
            rule_scope(state, auth, &q.into_scope()?).await?;
        }
        ["evidence", id] if read => {
            let e = state
                .evidence
                .get(parse_id(id.to_string(), "evidence")?)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(denied)?;
            rule_scope(state, auth, &e.scope).await?;
        }
        ["commands"] if req.method() == Method::POST => {
            let (parts, body) = req.into_parts();
            let bytes = to_bytes(body, 2 * 1024 * 1024)
                .await
                .map_err(|_| denied())?;
            let envelope: CommandRequest = serde_json::from_slice(&bytes).map_err(|_| denied())?;
            // Legacy replay keys are workspace-global and not bound to a principal.
            if envelope.client_command_id.is_some() {
                return Err(denied());
            }
            command(state, auth, &envelope.command).await?;
            return Ok(Request::from_parts(parts, Body::from(bytes)));
        }
        _ => return Err(denied()),
    }
    Ok(req)
}

async fn task(state: &AppState, auth: &AuthContext, id: TaskId) -> Result<(), ApiError> {
    let t = state
        .tasks
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(denied)?;
    require_project_scope(auth, t.project_id)?;
    for p in state
        .plans
        .list_plans_for_task(id)
        .await
        .map_err(ApiError::from)?
    {
        require_project_scope(auth, Some(p.project_id))?;
    }
    // Readiness and mutation warnings include related task titles.
    for relation in state
        .relations
        .list_by_task(id)
        .await
        .map_err(ApiError::from)?
    {
        for other in [relation.from, relation.to] {
            let t = state
                .tasks
                .get(other)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(denied)?;
            require_project_scope(auth, t.project_id)?;
        }
    }
    Ok(())
}

async fn plan(state: &AppState, auth: &AuthContext, id: PlanId) -> Result<(), ApiError> {
    let p = state
        .plans
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(denied)?;
    require_project_scope(auth, Some(p.project_id))?;
    if let Some(parent) = p.parent_plan_id {
        require_plan_scope(state, auth, parent).await?;
    }
    for child in state
        .plans
        .list_children(id)
        .await
        .map_err(ApiError::from)?
    {
        require_project_scope(auth, Some(child.project_id))?;
    }
    if let Some(source) = &p.source_ref {
        source_scope(state, auth, source).await?;
    }
    for member in state
        .plans
        .list_tasks_ordered(id)
        .await
        .map_err(ApiError::from)?
    {
        task(state, auth, member.task_id).await?;
        for dependency in member.depends_on {
            task(state, auth, dependency).await?;
        }
    }
    Ok(())
}

pub(super) async fn run(state: &AppState, auth: &AuthContext, id: RunId) -> Result<(), ApiError> {
    let mut next = Some(id);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            return Err(denied());
        }
        let r = state
            .runs
            .get(id)
            .await
            .map_err(ApiError::from)?
            .ok_or_else(denied)?;
        plan(state, auth, r.plan_id).await?;
        for step in state.runs.list_steps(id).await.map_err(ApiError::from)? {
            task(state, auth, step.task_id).await?;
        }
        next = r.parent_run_id;
    }
    Ok(())
}

async fn new_task(
    state: &AppState,
    auth: &AuthContext,
    t: &daruma_domain::NewTask,
    project: Option<ProjectId>,
) -> Result<(), ApiError> {
    require_project_scope(auth, project)?;
    if let Some(id) = t.id {
        if state.tasks.get(id).await.map_err(ApiError::from)?.is_some() {
            task(state, auth, id).await?;
        }
    }
    if let Some(key) = &t.external_key {
        if let Some(existing) = state
            .tasks
            .find_by_external_key(key.trim())
            .await
            .map_err(ApiError::from)?
        {
            task(state, auth, existing.id).await?;
        }
    }
    Ok(())
}

async fn command(state: &AppState, auth: &AuthContext, cmd: &Command) -> Result<(), ApiError> {
    use Command::*;
    match cmd {
        CreateTask { task: t } => new_task(state, auth, t, t.project_id).await?,
        UpdateTask { id, patch } => {
            task(state, auth, *id).await?;
            if let Some(project) = patch.project_id {
                require_project_scope(auth, project)?;
            }
        }
        CompleteTask { id, .. }
        | DeleteTask { id }
        | SetStatus { id, .. }
        | SetPriority { id, .. }
        | AdvancePipeline { id, .. }
        | ClosePipelineStage { id, .. } => task(state, auth, *id).await?,
        BulkSetStatus { ids, .. } => {
            for id in ids {
                task(state, auth, *id).await?;
            }
        }
        MaterializePlan {
            plan: p,
            tasks,
            plan_id,
        } => {
            new_plan(state, auth, p).await?;
            if let Some(id) = plan_id {
                plan(state, auth, *id).await?;
            }
            if let Some(id) = p.parent_plan_id {
                plan(state, auth, id).await?;
            }
            for t in tasks {
                new_task(state, auth, t, t.project_id.or(Some(p.project_id))).await?;
            }
        }
        CreatePlan {
            plan: p,
            external_ref,
        } => {
            if external_ref.is_some() {
                return Err(denied());
            }
            new_plan(state, auth, p).await?;
            if let Some(id) = p.parent_plan_id {
                plan(state, auth, id).await?;
            }
        }
        UpdatePlan { id, patch } => {
            plan(state, auth, *id).await?;
            if let Some(Some(parent)) = patch.parent_plan_id {
                plan(state, auth, parent).await?;
            }
        }
        AmendPlanTask {
            plan_id,
            task_id,
            patch,
        } => {
            plan(state, auth, *plan_id).await?;
            task(state, auth, *task_id).await?;
            if let Some(project) = patch.project_id {
                require_project_scope(auth, project)?;
            }
        }
        ArchivePlan { id } | DeletePlan { id } => plan(state, auth, *id).await?,
        SetPlanGoal { plan_id, .. } | SetPlanStatus { plan_id, .. } => {
            plan(state, auth, *plan_id).await?
        }
        AddPlanTask {
            plan_id,
            task_id,
            depends_on,
            ..
        } => {
            plan(state, auth, *plan_id).await?;
            task(state, auth, *task_id).await?;
            for id in depends_on.iter().flatten() {
                task(state, auth, *id).await?;
            }
        }
        RemovePlanTask { plan_id, task_id } => {
            plan(state, auth, *plan_id).await?;
            task(state, auth, *task_id).await?;
        }
        BulkAttachToPlan { plan_id, task_ids }
        | ReorderPlan {
            plan_id,
            order: task_ids,
        } => {
            plan(state, auth, *plan_id).await?;
            for id in task_ids {
                task(state, auth, *id).await?;
            }
        }
        AddComment { comment } => {
            task(state, auth, comment.task_id).await?;
            for id in [comment.id, comment.parent_id].into_iter().flatten() {
                if comment.id == Some(id)
                    && state
                        .comments
                        .get(id)
                        .await
                        .map_err(ApiError::from)?
                        .is_none()
                {
                    continue;
                }
                let c = state
                    .comments
                    .get(id)
                    .await
                    .map_err(ApiError::from)?
                    .ok_or_else(denied)?;
                task(state, auth, c.task_id).await?;
            }
        }
        EditComment { id, .. } | DeleteComment { id } => {
            let c = state
                .comments
                .get(*id)
                .await
                .map_err(ApiError::from)?
                .ok_or_else(denied)?;
            task(state, auth, c.task_id).await?;
        }
        LinkTasks { from, to, .. } => {
            task(state, auth, *from).await?;
            task(state, auth, *to).await?;
        }
        StartRun {
            plan_id,
            parent_run_id,
            ..
        } => {
            plan(state, auth, *plan_id).await?;
            if let Some(id) = parent_run_id {
                run(state, auth, *id).await?;
            }
        }
        RunStartStep { run_id, task_id }
        | RunFinishStep {
            run_id, task_id, ..
        } => {
            run(state, auth, *run_id).await?;
            task(state, auth, *task_id).await?;
        }
        CompleteRun { run_id }
        | FailRun { run_id, .. }
        | AbortRun { run_id, .. }
        | AppendRunNote { run_id, .. } => run(state, auth, *run_id).await?,
        AcquireClaim { task_id, .. }
        | ReleaseClaim { task_id, .. }
        | ReleaseFiles { task_id, .. } => task(state, auth, *task_id).await?,
        RecordEvidence { evidence: e } => {
            rule_scope(state, auth, &e.scope).await?;
            for id in [e.id, e.supersedes].into_iter().flatten() {
                if let Some(existing) = state.evidence.get(id).await.map_err(ApiError::from)? {
                    rule_scope(state, auth, &existing.scope).await?;
                }
            }
            if let Some(id) = e.project_id {
                require_project_scope(auth, Some(id))?;
            }
            if let Some(id) = e.plan_id {
                plan(state, auth, id).await?;
            }
            if let Some(id) = e.task_id {
                task(state, auth, id).await?;
            }
            if let Some(id) = e.run_id {
                run(state, auth, id).await?;
            }
            if e.artifact_id.is_some() || e.rule_id.is_some() {
                return Err(denied());
            }
        }
        UpdateProject { id, .. } | DeleteProject { id } => require_project_scope(auth, Some(*id))?,
        UpdateProjectSettings { project_id, .. } => require_project_scope(auth, Some(*project_id))?,
        _ => return Err(denied()),
    }
    Ok(())
}

async fn read_body<T: serde::de::DeserializeOwned>(req: Request) -> Result<(Request, T), ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024)
        .await
        .map_err(|_| denied())?;
    let parsed = serde_json::from_slice(&bytes).map_err(|_| denied())?;
    Ok((Request::from_parts(parts, Body::from(bytes)), parsed))
}

pub(super) async fn source_scope(
    state: &AppState,
    auth: &AuthContext,
    source: &str,
) -> Result<(), ApiError> {
    let source = daruma_domain::normalize_source_ref(source).map_err(|_| denied())?;
    for node in state
        .plans
        .source_chain(&source)
        .await
        .map_err(ApiError::from)?
    {
        let owners = state
            .plans
            .list_by_source(None, &node.source_ref)
            .await
            .map_err(ApiError::from)?;
        if owners.is_empty() {
            return Err(denied());
        }
        for owner in owners {
            require_project_scope(auth, Some(owner.project_id))?;
        }
    }
    Ok(())
}
async fn new_plan(state: &AppState, auth: &AuthContext, p: &NewPlan) -> Result<(), ApiError> {
    require_project_scope(auth, Some(p.project_id))?;
    let mut resolved = p.clone();
    // Reuse the source resolver, including project policy and inherited refs.
    // It builds events without persisting; never expose its global conflict data.
    let (_, events) = state
        .commands
        .handler()
        .resolve_plan_source(&mut resolved, &actor_from(auth, None))
        .await
        .map_err(|_| denied())?;
    if let Some(parent) = resolved.parent_plan_id {
        require_plan_scope(state, auth, parent).await?;
    }
    if let Some(source) = &resolved.source_ref {
        source_scope(state, auth, source).await?;
    }
    for event in events {
        if let Event::SourceUpserted { source } = event {
            source_scope(state, auth, &source.source_ref).await?;
            if let Some(upstream) = source.upstream_ref {
                source_scope(state, auth, &upstream).await?;
            }
        }
    }
    if let Some(source) = &p.source_ref {
        source_scope(state, auth, source).await?;
    }
    if let Some(source) = &p.source {
        let mut pending = vec![source];
        while let Some(node) = pending.pop() {
            if let Some(source) = &node.source_ref {
                source_scope(state, auth, source).await?;
            }
            pending.extend(&node.upstream);
        }
    }
    Ok(())
}

async fn rule_scope(
    state: &AppState,
    auth: &AuthContext,
    scope: &daruma_domain::RuleScope,
) -> Result<(), ApiError> {
    use daruma_domain::RuleScope;
    match scope {
        RuleScope::Tenant => Err(denied()),
        RuleScope::Project { id } => require_project_scope(auth, Some(*id)),
        RuleScope::Plan { id } => plan(state, auth, *id).await,
        RuleScope::Task { id } => task(state, auth, *id).await,
    }
}
