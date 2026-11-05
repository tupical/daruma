//! Task 01a1ab50-64c3-7e21-bd4b-39889b9a2086 — corporate-game scenario:
//! `daruma_split` keeps the provenance of the tasks it creates. Driven through
//! the hosted MCP endpoint `POST /v1/mcp?profile=full` of a real inline HTTP
//! server with plan-only intake and production lifecycle wiring
//! (`rule_engine_gate`).
//!
//! Story: the HR lead plans the quarterly tournament, then splits "Провести
//! турнир" into two stages and the first stage into two matches. Later the
//! auditor asks which split produced each task.
//!
//! Expected (stated before execution):
//! 1. `daruma_get` of every child returns `source_event_id` = the id of the
//!    `TaskSplitGenerated` that created it; the nested split's children name
//!    their own event, not the ancestor's;
//! 2. children sit in the parent's plan, with the parent's project;
//! 3. `GET /v1/events` holds `TaskSplitGenerated` before the children's
//!    `TaskCreated`, which carry the same `source_event_id`;
//! 4. the stored log replayed into a clean store gives the same ids;
//! 5. a task from `daruma_plan_materialize` still points at `PlanCreated`, and
//!    `daruma_set_status` does not change any task's `source_event_id`.

use daruma_events::EventEnvelope;
use daruma_storage::{Db, ProjectRepo, TaskRepo};
use serde_json::{json, Value};

mod common;
use common::{json_get, spawn_server, TestAppBuilder};

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
        serde_json::from_str(text).unwrap()
    }

    async fn source_event_id(&self, task_id: &str) -> Value {
        let task = self.tool("daruma_get", json!({"id": task_id})).await;
        println!(
            "TRACE daruma_get {task_id}: title={} project_id={} source_event_id={}",
            task["title"], task["project_id"], task["source_event_id"],
        );
        task["source_event_id"].clone()
    }

    async fn split(&self, parent: &str, titles: [&str; 2]) -> Vec<String> {
        let subtasks: Vec<Value> = titles.iter().map(|t| json!({"title": t})).collect();
        let result = self
            .tool(
                "daruma_split",
                json!({"parent": parent, "subtasks": subtasks}),
            )
            .await;
        println!(
            "TRACE daruma_split {parent}: event_seq={} success={}",
            result["event_seq"], result["success"]
        );
        result["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["payload"]["type"] == "task_created")
            .map(|e| e["payload"]["task"]["id"].as_str().unwrap().to_string())
            .collect()
    }
}

#[tokio::test]
async fn corporate_game_split_keeps_task_provenance() {
    let app = TestAppBuilder::default()
        .rule_engine_gate()
        .plan_only_intake(true)
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let mcp = Mcp {
        http: reqwest::Client::new(),
        url: format!("http://{addr}/v1/mcp?profile=full"),
        token: app.admin_token.clone(),
    };

    let project = mcp
        .tool(
            "daruma_project_create",
            json!({"title": "Корпоративная игра"}),
        )
        .await;
    let pid = project["project_id"].as_str().unwrap();
    let plan = mcp
        .tool(
            "daruma_plan_materialize",
            json!({
                "plan": {"title": "Квартальный турнир отделов", "project_id": pid,
                         "source": {"label": "планёрка HR"}},
                "tasks": [{"title": "Провести турнир"}]
            }),
        )
        .await;
    let plan_id = plan["plan_id"].as_str().unwrap().to_string();
    let tournament = plan["task_ids"][0].as_str().unwrap().to_string();

    let stages = mcp.split(&tournament, ["Отборочный этап", "Финал"]).await;
    assert_eq!(stages.len(), 2);
    let matches = mcp.split(&stages[0], ["Матч А", "Матч Б"]).await;
    assert_eq!(matches.len(), 2);

    let (status, log) = json_get(
        app.router.clone(),
        &app.admin_token,
        "/v1/events?since=0&limit=1000",
    )
    .await;
    assert_eq!(status, 200, "{log}");
    let log: Vec<EventEnvelope> = serde_json::from_value(log).unwrap();
    for env in &log {
        println!(
            "TRACE log seq={} {} id={}",
            env.seq,
            env.payload.kind(),
            env.id
        );
    }

    let splits: Vec<_> = log
        .iter()
        .filter(|e| e.payload.kind() == "task_split_generated")
        .collect();
    assert_eq!(splits.len(), 2);
    let plan_created = log
        .iter()
        .find(|e| e.payload.kind() == "plan_created")
        .unwrap();

    let expected = [
        (&tournament, json!(plan_created.id)),
        (&stages[0], json!(splits[0].id)),
        (&stages[1], json!(splits[0].id)),
        (&matches[0], json!(splits[1].id)),
        (&matches[1], json!(splits[1].id)),
    ];
    for (task_id, source) in &expected {
        assert_eq!(&mcp.source_event_id(task_id).await, source, "{task_id}");
    }

    // Membership and project inheritance survive.
    let in_plan = mcp.tool("daruma_plan_get", json!({"id": plan_id})).await;
    println!("TRACE daruma_plan_get progress={}", in_plan["progress"]);
    assert_eq!(in_plan["progress"]["tasks_total"], 5, "{in_plan}");
    for child in stages.iter().chain(&matches) {
        let task = mcp.tool("daruma_get", json!({"id": child})).await;
        assert_eq!(task["project_id"].as_str(), Some(pid), "{task}");
    }

    // The stored event carries the link itself, after the split event.
    let created_after_split = |task_id: &str, split_seq: u64| {
        let env = log
            .iter()
            .find(|e| {
                e.payload.kind() == "task_created"
                    && serde_json::to_value(&e.payload).unwrap()["task"]["id"] == task_id
            })
            .unwrap();
        assert!(env.seq > split_seq);
        serde_json::to_value(&env.payload).unwrap()["task"]["source_event_id"].clone()
    };
    assert_eq!(
        created_after_split(&stages[1], splits[0].seq),
        json!(splits[0].id)
    );

    // A later ordinary edit leaves provenance alone.
    mcp.tool(
        "daruma_set_status",
        json!({"id": stages[1], "status": "todo"}),
    )
    .await;
    assert_eq!(&mcp.source_event_id(&stages[1]).await, &expected[2].1);

    // Replay of everything stored into a clean store gives the same links.
    let stored = app.event_store().load_since(0, 1000).await.unwrap();
    let db = Db::memory().await.unwrap();
    db.migrate().await.unwrap();
    let clean_projects = ProjectRepo::new(db.pool().clone());
    let clean = TaskRepo::new(db.pool().clone());
    for env in &stored {
        clean_projects.apply_event(env).await.unwrap();
        clean.apply_event(env).await.unwrap();
    }
    for (task_id, source) in &expected {
        let task = clean.get(task_id.parse().unwrap()).await.unwrap().unwrap();
        let actual = json!(task.source_event_id);
        println!("TRACE replay {task_id}: source_event_id={actual}");
        assert_eq!(&actual, source, "replay {task_id}");
    }
}
