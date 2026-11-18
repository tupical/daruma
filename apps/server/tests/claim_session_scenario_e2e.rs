//! Task 01a057ce-ba17-7dd3-b562-0c98f6f9844b — corporate-game scenario: a
//! claim names the chat that took it, so two chats of one agent no longer
//! read as "the agent's latest session". Driven through the hosted MCP
//! endpoint `POST /v1/mcp` of a real inline HTTP server with production
//! lifecycle wiring (`rule_engine_gate`).
//!
//! Story: the organiser of a corporate game runs two chats under one token,
//! one preparing the brackets and one the prizes; a colleague works under a
//! separate token.
//!
//! Expected (stated before execution):
//! 1. chat A claims the brackets task and chat B, started later, drains the
//!    prizes task; each task's `current_claim.session` names its own chat and
//!    branch even though B is the agent's latest session;
//! 2. the transcript path given to `daruma_session_start` never appears in a
//!    read of the task;
//! 3. a claim that names no session reads `session_id: null`, `session: null`:
//!    the owner is not guessed;
//! 4. renewing a claim without a session keeps the binding;
//! 5. a claim naming a colleague's session, an unknown session or an ended
//!    session is refused and leaves the task unclaimed.

use daruma_auth::{Capabilities, Capability, ProjectFilter};
use serde_json::{json, Value};

mod common;
use common::{mint_pat, spawn_server, TestAppBuilder};

struct Mcp {
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Mcp {
    async fn call(&self, name: &str, arguments: Value) -> Value {
        let body: Value = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body.get("error").is_none(), "{name}: {body}");
        body["result"].clone()
    }

    async fn tool(&self, name: &str, arguments: Value) -> Value {
        let result = self.call(name, arguments).await;
        assert_ne!(result["isError"], true, "{name}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    async fn refused(&self, name: &str, arguments: Value) -> String {
        let result = self.call(name, arguments).await;
        assert_eq!(result["isError"], true, "{name} must be refused: {result}");
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        println!("TRACE tools/call {name} refused: {text}");
        text
    }

    async fn claim_of(&self, task_id: &str) -> Value {
        let claim = self.tool("daruma_get", json!({"id": task_id})).await["current_claim"].clone();
        println!("TRACE current_claim {task_id}: {claim}");
        claim
    }

    async fn chat(&self, chat_id: &str, branch: &str) -> String {
        let started = self
            .tool(
                "daruma_session_start",
                json!({"metadata": {
                    "client": "claude-code",
                    "chat_id": chat_id,
                    "transcript_path": format!("/home/organiser/.chats/{chat_id}.jsonl"),
                    "git_work_context": {
                        "branch_ref": format!("refs/heads/{branch}"),
                        "worktree_path": format!("/srv/game/{branch}"),
                        "merge_request_id": null,
                    },
                }}),
            )
            .await;
        started
            .pointer("/data/session/id")
            .or_else(|| started.pointer("/session/id"))
            .or_else(|| started.pointer("/data/id"))
            .or_else(|| started.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("session id in {started}"))
            .to_string()
    }
}

fn task_ids(value: &Value) -> Vec<String> {
    serde_json::from_value(value["task_ids"].clone()).unwrap()
}

#[tokio::test]
async fn corporate_game_claims_stay_with_their_own_chats() {
    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/v1/mcp?profile=full");
    let organiser = Mcp {
        http: http.clone(),
        url: url.clone(),
        token: app.admin_token.clone(),
    };
    let caps: Capabilities = [
        Capability::TaskRead,
        Capability::TaskWrite,
        Capability::PlanRead,
        Capability::PlanWrite,
        Capability::ProjectRead,
        Capability::RunWrite,
        Capability::AgentDispatch,
    ]
    .into();
    let (colleague_token, _) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;
    let colleague = Mcp {
        http,
        url,
        token: colleague_token,
    };

    let project = organiser
        .tool(
            "daruma_project_create",
            json!({"title": "Корпоративная игра"}),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap();
    let plan = organiser
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Подготовка турнира", "project_id": pid,
                         "source": {"label": "планёрка HR"}},
                "tasks": [
                    {"title": "Сетка турнира"},
                    {"title": "Призы турнира"},
                    {"title": "Рассылка участникам"},
                    {"title": "Зал и инвентарь"}
                ],
            }),
        )
        .await;
    let tasks = task_ids(&plan);
    let plan_id = plan["plan"]["id"]
        .as_str()
        .or_else(|| plan["plan_id"].as_str())
        .unwrap_or_else(|| panic!("plan id in {plan}"))
        .to_string();
    let (brackets, prizes, mailing, hall) = (&tasks[0], &tasks[1], &tasks[2], &tasks[3]);
    for task_id in &tasks {
        organiser
            .tool(
                "daruma_set_status",
                json!({"id": task_id, "status": "todo"}),
            )
            .await;
    }

    let chat_a = organiser.chat("chat-brackets", "feature/brackets").await;
    let chat_b = organiser.chat("chat-prizes", "feature/prizes").await;

    // 1. Each chat's claim keeps its own session, not the latest one.
    let claimed = organiser
        .tool(
            "daruma_claim",
            json!({"task_id": brackets, "ttl_secs": 600, "session_id": chat_a}),
        )
        .await;
    println!("TRACE daruma_claim brackets: {claimed}");
    let drained = organiser
        .tool(
            "daruma_plan_drain_next",
            json!({"plan_id": plan_id, "claim_ttl_secs": 600, "session_id": chat_b}),
        )
        .await;
    println!("TRACE daruma_plan_drain_next: {drained}");
    assert_eq!(drained["task_id"], *prizes, "{drained}");

    let brackets_claim = organiser.claim_of(brackets).await;
    assert_eq!(brackets_claim["session_id"], chat_a, "{brackets_claim}");
    assert_eq!(
        brackets_claim["session"]["chat_id"], "chat-brackets",
        "{brackets_claim}"
    );
    assert_eq!(
        brackets_claim["session"]["branch_ref"], "refs/heads/feature/brackets",
        "{brackets_claim}"
    );
    let prizes_claim = organiser.claim_of(prizes).await;
    assert_eq!(prizes_claim["session_id"], chat_b, "{prizes_claim}");
    assert_eq!(
        prizes_claim["session"]["chat_id"], "chat-prizes",
        "{prizes_claim}"
    );

    // 2. Local paths of the client stay out of a task read.
    assert!(
        !brackets_claim.to_string().contains("/home/organiser"),
        "{brackets_claim}"
    );

    // 3. No session named: nothing is guessed.
    organiser
        .tool("daruma_claim", json!({"task_id": mailing, "ttl_secs": 600}))
        .await;
    let unbound = organiser.claim_of(mailing).await;
    assert_eq!(unbound["session_id"], Value::Null, "{unbound}");
    assert_eq!(unbound["session"], Value::Null, "{unbound}");

    // 4. Renewing without a session keeps the binding.
    organiser
        .tool(
            "daruma_claim",
            json!({"task_id": brackets, "ttl_secs": 900}),
        )
        .await;
    let renewed = organiser.claim_of(brackets).await;
    assert_eq!(renewed["session_id"], chat_a, "{renewed}");

    // 5. Refused: a colleague's session, an unknown one, an ended one.
    let foreign = colleague
        .refused(
            "daruma_claim",
            json!({"task_id": hall, "ttl_secs": 600, "session_id": chat_a}),
        )
        .await;
    assert!(foreign.contains("different agent"), "{foreign}");
    let unknown = organiser
        .refused(
            "daruma_claim",
            json!({"task_id": hall, "ttl_secs": 600,
                   "session_id": "00000000-0000-7000-8000-000000000000"}),
        )
        .await;
    assert!(
        unknown.contains("not_found") || unknown.contains("404"),
        "{unknown}"
    );
    let chat_c = organiser.chat("chat-finished", "feature/hall").await;
    organiser
        .tool("daruma_session_end", json!({"id": chat_c}))
        .await;
    let ended = organiser
        .refused(
            "daruma_claim",
            json!({"task_id": hall, "ttl_secs": 600, "session_id": chat_c}),
        )
        .await;
    assert!(ended.contains("already ended"), "{ended}");
    assert_eq!(organiser.claim_of(hall).await, Value::Null);
}

#[tokio::test]
async fn corporate_game_expected_drain_keeps_plan_order() {
    use daruma_domain::RunStatus;
    use daruma_shared::{PlanId, RunId, TaskId};

    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let organiser = Mcp {
        http: reqwest::Client::new(),
        url: format!("http://{addr}/v1/mcp?profile=full"),
        token: app.admin_token.clone(),
    };
    let project = organiser
        .tool(
            "daruma_project_create",
            json!({"title": "Корпоративная игра"}),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap();
    let plan = organiser
        .tool("daruma_plan_materialize", json!({
            "plan": {"title": "Подготовка турнира", "project_id": pid, "source": {"label": "планёрка HR"}},
            "tasks": [{"title": "Сетка турнира"}, {"title": "Призы турнира"}, {"title": "Правила турнира"}]
        }))
        .await;
    let plan_id: PlanId = plan["plan"]["id"]
        .as_str()
        .or_else(|| plan["plan_id"].as_str())
        .unwrap_or_else(|| panic!("plan id in {plan}"))
        .parse()
        .unwrap();
    let tasks = task_ids(&plan);
    let [brackets, prizes, rules] = tasks.as_slice() else {
        panic!("{tasks:?}")
    };
    for task in &tasks {
        organiser
            .tool("daruma_set_status", json!({"id": task, "status": "todo"}))
            .await;
    }
    let caps: Capabilities = [Capability::PlanRead, Capability::RunWrite].into();
    let project_filter = ProjectFilter::Only {
        projects: vec![pid.parse().unwrap()],
    };
    let (colleague_token, _) = mint_pat(&app.auth_store(), caps, project_filter).await;
    let colleague = Mcp {
        http: organiser.http.clone(),
        url: organiser.url.clone(),
        token: colleague_token,
    };
    let drain = "daruma_plan_drain_next";
    let progress = organiser
        .tool("daruma_plan_progress", json!({"plan_id": plan_id}))
        .await;
    assert_eq!(progress["next_ready"], *brackets, "{progress}");
    println!("TRACE tournament plan {plan_id}: observed {brackets}");

    // Each change between preview and drain must leave every task unclaimed.
    let expect_changed = |phase: &'static str, expected: &str, opens_run: bool| {
        let args = json!({"plan_id": plan_id, "expected_task_id": expected});
        let app = &app;
        let organiser = &organiser;
        async move {
            let before = app.state.runs.list_by_plan(plan_id).await.unwrap();
            let err = organiser.refused(drain, args).await;
            assert!(err.contains("selection_changed"), "{phase}: {err}");
            assert!(
                app.state.claims.list_active(None).await.unwrap().is_empty(),
                "{phase}"
            );
            let runs = app.state.runs.list_by_plan(plan_id).await.unwrap();
            assert!(
                runs.iter().all(|run| run.status == RunStatus::Aborted),
                "{phase}: {runs:?}"
            );
            let new_runs: Vec<_> = runs
                .iter()
                .filter(|run| !before.iter().any(|old| old.id == run.id))
                .map(|run| run.id)
                .collect();
            assert_eq!(
                new_runs.len(),
                usize::from(opens_run),
                "{phase}: {new_runs:?}"
            );
            println!("TRACE {phase} refused; new aborted runs: {new_runs:?}");
        }
    };
    expect_changed("out of order", prizes, true).await;
    organiser
        .tool(
            "daruma_plan_reorder",
            json!({"plan_id": plan_id, "order": [prizes, brackets, rules]}),
        )
        .await;
    expect_changed("reordered", brackets, true).await;
    organiser
        .tool(
            "daruma_plan_reorder",
            json!({"plan_id": plan_id, "order": tasks}),
        )
        .await;

    organiser
        .tool(
            "daruma_plan_remove_task",
            json!({"plan_id": plan_id, "task_id": brackets}),
        )
        .await;
    expect_changed("membership removed", brackets, true).await;
    organiser
        .tool(
            "daruma_plan_add_task",
            json!({
                "plan_id": plan_id, "task_id": brackets, "position": 0, "depends_on": [rules]
            }),
        )
        .await;
    expect_changed("pending prerequisite", brackets, true).await;
    organiser
        .tool(
            "daruma_plan_remove_task",
            json!({"plan_id": plan_id, "task_id": brackets}),
        )
        .await;
    organiser
        .tool(
            "daruma_plan_add_task",
            json!({"plan_id": plan_id, "task_id": brackets, "position": 0}),
        )
        .await;

    let relation = organiser
        .tool(
            "daruma_link",
            json!({"from": rules, "to": brackets, "kind": "blocks"}),
        )
        .await;
    expect_changed("active Blocks", brackets, true).await;
    organiser
        .tool(
            "daruma_unlink",
            json!({"relation_id": relation["data"]["relation_id"]}),
        )
        .await;

    let ended = organiser
        .chat("tournament-ended", "feature/tournament")
        .await;
    organiser
        .tool("daruma_session_end", json!({"id": ended}))
        .await;
    let err = organiser
        .refused(
            drain,
            json!({
                "plan_id": plan_id, "expected_task_id": brackets, "session_id": ended
            }),
        )
        .await;
    assert!(err.contains("already ended"), "{err}");
    assert!(app.state.claims.list_active(None).await.unwrap().is_empty());
    assert!(app
        .state
        .runs
        .list_active_for_plan(plan_id)
        .await
        .unwrap()
        .is_empty());

    let supplied = organiser
        .tool("daruma_run_start", json!({"plan_id": plan_id}))
        .await;
    let run: RunId = serde_json::from_value(supplied["data"]["run_id"].clone()).unwrap();
    let err = colleague
        .refused(
            drain,
            json!({
                "plan_id": plan_id, "run_id": run, "expected_task_id": brackets
            }),
        )
        .await;
    assert!(err.contains("authenticated owner"), "{err}");
    let err = organiser
        .refused(
            drain,
            json!({
                "plan_id": plan_id, "run_id": run, "expected_task_id": prizes
            }),
        )
        .await;
    assert!(err.contains("selection_changed"), "{err}");
    assert_eq!(
        app.state.runs.get(run).await.unwrap().unwrap().status,
        RunStatus::Active
    );
    assert!(app.state.claims.list_active(None).await.unwrap().is_empty());
    println!("TRACE rejected supplied run {run} stays active");

    // Two workers target the same observed head. Only one wins and the loser
    // cannot fall through to prizes even though it is ready.
    let (owner, other) = tokio::join!(
        organiser.call(
            drain,
            json!({"plan_id": plan_id, "run_id": run, "expected_task_id": brackets})
        ),
        colleague.call(
            drain,
            json!({"plan_id": plan_id, "expected_task_id": brackets})
        ),
    );
    let (winner, loser, unclaimed_worker) = if owner["isError"] == true {
        (&other, &owner, &organiser)
    } else {
        (&owner, &other, &colleague)
    };
    let won: Value = serde_json::from_str(winner["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(won["task_id"], *brackets, "{won}");
    assert_eq!(loser["isError"], true, "{loser}");
    assert!(loser.to_string().contains("selection_changed"), "{loser}");
    let claims = app.state.claims.list_active(None).await.unwrap();
    assert_eq!(claims.len(), 1, "{claims:?}");
    assert_eq!(claims[0].task_id, brackets.parse::<TaskId>().unwrap());
    let winning_run: RunId = serde_json::from_value(won["run_id"].clone()).unwrap();
    let active = app.state.runs.list_active_for_plan(plan_id).await.unwrap();
    assert_eq!(
        active.len(),
        if winning_run == run { 1 } else { 2 },
        "{active:?}"
    );
    assert!(
        active
            .iter()
            .all(|active| [run, winning_run].contains(&active.id)),
        "{active:?}"
    );
    assert_eq!(
        organiser.tool("daruma_get", json!({"id": prizes})).await["status"],
        "todo"
    );
    println!("TRACE concurrent winner: {won}; concurrent loser: {loser}");

    let legacy = unclaimed_worker
        .tool(drain, json!({"plan_id": plan_id}))
        .await;
    assert_eq!(legacy["task_id"], *prizes, "{legacy}");
    println!("TRACE legacy drain: {legacy}");
    for active in app.state.runs.list_active_for_plan(plan_id).await.unwrap() {
        organiser
            .tool(
                "daruma_run_abort",
                json!({"run_id": active.id, "reason": "test cleanup"}),
            )
            .await;
    }
    organiser
        .tool(
            "daruma_set_status",
            json!({"id": brackets, "status": "cancelled"}),
        )
        .await;
    expect_changed("terminal status", brackets, true).await;
    organiser
        .tool("daruma_plan_archive", json!({"id": plan_id}))
        .await;
    expect_changed("archived plan", rules, false).await;
}
