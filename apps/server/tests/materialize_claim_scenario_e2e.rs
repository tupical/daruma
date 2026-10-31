//! Task 01a159c4-9cec-7103-9a26-20a3a429d4ca — corporate-game scenario:
//! `daruma_plan_materialize` with `claim` creates the plan and claims its next
//! ready task in one call, and reports a failed claim as an explicit partial
//! outcome instead of pretending the two steps are atomic. Driven through the
//! hosted MCP endpoint `POST /v1/mcp` (the route the cloud gateway proxies to)
//! of a real inline HTTP server with production lifecycle wiring
//! (`rule_engine_gate`).
//!
//! Story: the HR lead of a corporate game plans the tournament with a token
//! that may write plans but not start runs; the executor agent plans its own
//! work and starts it in the same call, then picks up the HR lead's plan.
//!
//! Expected (stated before execution):
//! 1. HR lead materializes with `claim`: the plan and both tasks exist,
//!    `claim = {outcome: not_claimed, reason: <HTTP 403 …>, plan_id}`,
//!    `next_tool = daruma_plan_drain_next {plan_id}` (the retry), tasks `todo`;
//! 2. executor materializes with `claim {ttl_secs: 600}`: `claim = {outcome:
//!    claimed, task_id = first task, in_batch: true, run_id, claim_id}`,
//!    `task {status: in_progress, claim_expires_at}`, `next_tool =
//!    daruma_comment {task_id, kind: outcome}`; the second task stays `todo`;
//! 3. without `claim` materialize is unchanged: no `claim` field, next step
//!    is draining;
//! 4. the executor follows the HR lead's retry hint and gets the HR lead's
//!    first task — the plan from step 1 was never lost.

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
    async fn tool(&self, name: &str, arguments: Value) -> Value {
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
        let result = &body["result"];
        assert_ne!(result["isError"], true, "{name}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        println!(
            "TRACE tools/call {name}: claim={} task={} next_tool={} task_ids={}",
            value.get("claim").unwrap_or(&Value::Null),
            value.get("task").unwrap_or(&Value::Null),
            value.get("next_tool").unwrap_or(&Value::Null),
            value.get("task_ids").unwrap_or(&Value::Null),
        );
        value
    }

    async fn status(&self, task_id: &str) -> Value {
        self.tool("daruma_get", json!({"id": task_id})).await["status"].clone()
    }
}

fn ids(value: &Value) -> Vec<String> {
    serde_json::from_value(value["task_ids"].clone()).unwrap()
}

#[tokio::test]
async fn corporate_game_materialize_and_claim_in_one_call() {
    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/v1/mcp?profile=full");
    let executor = Mcp {
        http: http.clone(),
        url: url.clone(),
        token: app.admin_token.clone(),
    };
    // Plans and tasks, but no runs: the drain half of the call is refused.
    let caps: Capabilities = [
        Capability::TaskRead,
        Capability::TaskWrite,
        Capability::PlanRead,
        Capability::PlanWrite,
        Capability::ProjectRead,
    ]
    .into();
    let (hr_token, _) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;
    let hr_lead = Mcp {
        http,
        url,
        token: hr_token,
    };

    let project = executor
        .tool(
            "daruma_project_create",
            json!({"title": "Корпоративная игра"}),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap();

    // 1. Partial outcome: plan created, claim refused.
    let tournament = hr_lead
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Квартальный турнир отделов", "project_id": pid,
                         "source": {"label": "планёрка HR"}},
                "tasks": [
                    {"title": "Собрать команды отделов"},
                    {"title": "Подвести итоги турнира"}
                ],
                "claim": {"ttl_secs": 600}
            }),
        )
        .await;
    let tournament_tasks = ids(&tournament);
    assert_eq!(tournament_tasks.len(), 2, "{tournament}");
    let claim = &tournament["claim"];
    assert_eq!(claim["outcome"], "not_claimed", "{tournament}");
    assert!(
        claim["reason"].as_str().unwrap().contains("403"),
        "{tournament}"
    );
    let tournament_plan = claim["plan_id"].as_str().unwrap().to_string();
    assert_eq!(
        tournament["next_tool"],
        json!({"name": "daruma_plan_drain_next", "arguments": {"plan_id": tournament_plan}})
    );
    for task_id in &tournament_tasks {
        assert_eq!(executor.status(task_id).await, "todo");
    }

    // 2. Plan and start in one call.
    let prizes = executor
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Призовой фонд турнира", "project_id": pid,
                         "source": {"label": "чат с HR"}},
                "tasks": [
                    {"title": "Согласовать бюджет призов"},
                    {"title": "Закупить кубки"}
                ],
                "claim": {"ttl_secs": 600}
            }),
        )
        .await;
    let prize_tasks = ids(&prizes);
    let claim = &prizes["claim"];
    assert_eq!(claim["outcome"], "claimed", "{prizes}");
    assert_eq!(claim["task_id"], prize_tasks[0].as_str());
    assert_eq!(claim["in_batch"], true);
    assert!(claim["run_id"].is_string(), "{prizes}");
    assert!(claim["claim_id"].is_string(), "{prizes}");
    assert_eq!(prizes["task"]["id"], prize_tasks[0].as_str());
    assert_eq!(prizes["task"]["status"], "in_progress");
    assert!(prizes["task"]["claim_expires_at"].is_string(), "{prizes}");
    assert_eq!(
        prizes["next_tool"],
        json!({"name": "daruma_comment",
               "arguments": {"task_id": prize_tasks[0], "kind": "outcome"}})
    );
    assert_eq!(executor.status(&prize_tasks[1]).await, "todo");

    // 3. No `claim`: the call is the plain materialize it always was.
    let plain = executor
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Фотоотчёт турнира", "project_id": pid,
                         "source": {"label": "просьба маркетинга"}},
                "tasks": [{"title": "Найти фотографа"}]
            }),
        )
        .await;
    assert!(plain.get("claim").is_none(), "{plain}");
    assert_eq!(plain["next_tool"]["name"], "daruma_plan_drain_next");
    assert_eq!(executor.status(&ids(&plain)[0]).await, "todo");

    // 4. The partial outcome's retry hint still leads to the HR lead's plan.
    let mut retry = tournament["next_tool"]["arguments"].clone();
    retry["claim_ttl_secs"] = json!(600);
    let drained = executor.tool("daruma_plan_drain_next", retry).await;
    assert_eq!(drained["task"]["id"], tournament_tasks[0].as_str());
    assert_eq!(drained["task"]["status"], "in_progress");
    println!(
        "TRACE side effects: tournament_plan={tournament_plan} tournament_tasks={tournament_tasks:?} \
         prize_tasks={prize_tasks:?} prize_run={}",
        prizes["claim"]["run_id"]
    );
}
