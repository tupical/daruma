//! Task 01a17eb7-…-ba8af15694b8 (T3) — corporate-game scenario: a mutation's
//! result leads to the next lifecycle step, and `initialize.instructions`
//! names the agent's open tasks. Driven through the hosted MCP endpoint
//! `POST /v1/mcp` (the route the cloud gateway proxies to) of a real inline
//! HTTP server with production lifecycle wiring (`rule_engine_gate`).
//!
//! Story: the HR lead of a corporate game decides the quarter's work and
//! materializes it; an executor agent drains a task, reports the outcome and
//! completes it; a second agent of the same workspace is watching.
//!
//! Expected (stated before execution):
//! 1. `initialize` of the executor before any work: no open-tasks line,
//!    instructions carry the T2 rule and are ≤ 2048 chars;
//! 2. materialize → `task_ids` (2, creation order) and
//!    `next_tool = daruma_plan_drain_next {plan_id}`;
//! 3. calling exactly that `next_tool` (plus a TTL) → `task {id, status:
//!    in_progress, claim_expires_at}` and `next_tool = daruma_comment
//!    {task_id, kind: outcome}`;
//! 4. `initialize` of the executor names that task, its title and
//!    `in_progress`; the second agent's `initialize` names nothing;
//! 5. outcome comment → `task.status = in_review`,
//!    `next_tool = daruma_complete {id}`; `initialize` shows `in_review`;
//! 6. `daruma_complete` with the suggested arguments → `task.status = done`,
//!    `next_tool = null`; `initialize` has no open-tasks line again.

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
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let body: Value = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(body.get("error").is_none(), "{method}: {body}");
        body["result"].clone()
    }

    async fn tool(&self, name: &str, arguments: Value) -> Value {
        let result = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        assert_ne!(result["isError"], true, "{name}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        let value: Value = serde_json::from_str(text).unwrap();
        println!(
            "TRACE tools/call {name}: task={} next_tool={} task_ids={}",
            value.get("task").unwrap_or(&Value::Null),
            value.get("next_tool").unwrap_or(&Value::Null),
            value.get("task_ids").unwrap_or(&Value::Null),
        );
        value
    }

    /// `instructions` and its open-tasks line (last paragraph), if any.
    async fn initialize(&self, who: &str) -> (String, Option<String>) {
        let result = self.rpc("initialize", json!({})).await;
        let text = result["instructions"].as_str().unwrap().to_string();
        let line = text
            .rsplit("\n\n")
            .next()
            .filter(|p| p.starts_with(daruma_mcp::OPEN_TASKS_MARKER))
            .map(str::to_string);
        println!(
            "TRACE initialize[{who}]: chars={} open_tasks={line:?} meta={}",
            text.chars().count(),
            result
                .pointer(&format!(
                    "/_meta/{}",
                    daruma_mcp::OPEN_TASKS_META_KEY.replace('/', "~1")
                ))
                .unwrap_or(&Value::Null)
        );
        assert!(text.chars().count() <= daruma_mcp::MAX_INSTRUCTIONS_CHARS);
        (text, line)
    }

    /// Call a suggested `next_tool` as-is, plus the given extra arguments.
    async fn follow(&self, next_tool: &Value, extra: Value) -> Value {
        let mut arguments = next_tool["arguments"].clone();
        for (k, v) in extra.as_object().unwrap() {
            arguments[k] = v.clone();
        }
        self.tool(next_tool["name"].as_str().unwrap(), arguments)
            .await
    }
}

#[tokio::test]
async fn corporate_game_mutations_lead_to_the_next_step() {
    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/v1/mcp?profile=full");
    let executor = Mcp {
        http: http.clone(),
        url: url.clone(),
        token: app.admin_token.clone(),
    };
    let caps: Capabilities = [
        Capability::TaskRead,
        Capability::TaskWrite,
        Capability::RunRead,
        Capability::RunWrite,
    ]
    .into();
    let (watcher_token, _) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;
    let watcher = Mcp {
        http,
        url,
        token: watcher_token,
    };

    // 1. Nothing held yet.
    let (text, line) = executor.initialize("executor").await;
    assert_eq!(line, None);
    assert!(text.contains(
        "claim or intent/progress comment → in_progress; outcome comment → in_review; daruma_complete → done"
    ));

    // 2. The HR lead decides the quarter's work.
    let project = executor
        .tool(
            "daruma_project_create",
            json!({"title": "Корпоративная игра"}),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap();
    let plan = executor
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Квартальный турнир отделов", "project_id": pid},
                "tasks": [
                    {"title": "Собрать команды отделов"},
                    {"title": "Подвести итоги турнира"}
                ]
            }),
        )
        .await;
    let task_ids: Vec<String> = serde_json::from_value(plan["task_ids"].clone()).unwrap();
    assert_eq!(task_ids.len(), 2, "{plan}");
    let next = plan["next_tool"].clone();
    assert_eq!(next["name"], "daruma_plan_drain_next");
    let plan_id = next["arguments"]["plan_id"].as_str().unwrap().to_string();

    // 3. The executor follows the hint.
    let drained = executor.follow(&next, json!({"claim_ttl_secs": 600})).await;
    let task_id = task_ids[0].as_str();
    assert_eq!(drained["task"]["id"], task_id);
    assert_eq!(drained["task"]["status"], "in_progress");
    assert!(drained["task"]["claim_expires_at"].is_string(), "{drained}");
    assert_eq!(
        drained["next_tool"],
        json!({"name": "daruma_comment", "arguments": {"task_id": task_id, "kind": "outcome"}})
    );

    // 4. A new session of the executor starts knowing its open task.
    let (_, line) = executor.initialize("executor").await;
    assert_eq!(
        line.as_deref(),
        Some(
            format!(
                "{}{task_id} — \"Собрать команды отделов\" — in_progress",
                daruma_mcp::OPEN_TASKS_MARKER
            )
            .as_str()
        )
    );
    let (_, line) = watcher.initialize("watcher").await;
    assert_eq!(
        line, None,
        "another agent's claim is not the watcher's task"
    );

    // 5. Outcome.
    let commented = executor
        .follow(
            &drained["next_tool"],
            json!({"body": "Команды собраны: 6 отделов, капитаны назначены."}),
        )
        .await;
    assert_eq!(commented["task"]["status"], "in_review");
    assert_eq!(
        commented["next_tool"],
        json!({"name": "daruma_complete", "arguments": {"id": task_id}})
    );
    let (_, line) = executor.initialize("executor").await;
    assert!(line.unwrap().ends_with("— in_review"));

    // 6. Completion ends the lifecycle.
    let completed = executor.follow(&commented["next_tool"], json!({})).await;
    assert_eq!(completed["task"]["status"], "done");
    assert_eq!(completed["next_tool"], Value::Null);
    let (_, line) = executor.initialize("executor").await;
    assert_eq!(line, None);

    // Side effects in Daruma: the second task is untouched, the plan open.
    let other = executor
        .tool("daruma_get", json!({"id": task_ids[1]}))
        .await;
    println!(
        "TRACE side effects: plan={plan_id} second_task_status={}",
        other["status"]
    );
    assert_eq!(other["status"], "todo");
}
