//! Task 01a14418-…-e972af88e2c4 — corporate-game reproduction of concurrent
//! retries that share one `client_command_id`, driven over a real inline HTTP
//! server: the game's bot module posts `/v1/commands`, an agent calls MCP
//! `daruma_link`.
//!
//! Before the fix the idempotency key was written only after the mutation ran,
//! so a burst of retries (flaky network, client timeout) executed the command
//! once per retry.
//!
//! Expected (stated before execution):
//! 1. six concurrent `create_task` retries with one ccid → all six succeed
//!    with one `event_id`, and the project holds exactly one such task;
//! 2. a `create_task` blocked by a `task.created` rule does not consume its
//!    ccid: after evidence is submitted the retry with the same ccid runs
//!    and creates the task;
//! 3. six concurrent `daruma_link` calls with one ccid → one relation, one
//!    `task.linked` event; `daruma_relations` lists a single `blocks` edge.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

async fn dispatch(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "tool {name}: {:?}", resp.error);
    resp.result.unwrap()
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let result = dispatch(client, name, arguments).await;
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    serde_json::from_str(text).unwrap()
}

/// The `unblock` array a `rule_blocked` error ends with.
fn unblock_hints(err: &str) -> Vec<Value> {
    let body: Value = err
        .find('{')
        .and_then(|at| serde_json::from_str(&err[at..]).ok())
        .unwrap_or_else(|| panic!("error must carry a JSON body: {err}"));
    let message = body["error"]["message"].as_str().unwrap_or_default();
    let (_, suffix) = message
        .rsplit_once(" | unblock: ")
        .unwrap_or_else(|| panic!("blocked error must carry an unblock hint: {err}"));
    serde_json::from_str(suffix).unwrap_or_else(|e| panic!("unblock JSON ({e}): {suffix}"))
}

async fn count(pool: &sqlx::SqlitePool, sql: &str, bind: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(bind)
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

fn create_task_command(pid: &str, title: &str, ccid: uuid::Uuid) -> Value {
    json!({
        "command": { "type": "create_task", "task": { "title": title, "project_id": pid } },
        "client_command_id": ccid,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corporate_game_retry_burst_executes_once() {
    let app = TestAppBuilder::default().rule_engine_gate().build().await;
    let addr = spawn_server(&app).await;
    let base = format!("http://{addr}");
    let agent = ApiClient::new(base.clone(), app.admin_token.clone());

    let project = call_tool(
        &agent,
        "daruma_project_create",
        json!({ "title": "Corporate game" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    eprintln!("TRACE setup project={pid}");

    // 1. The bot module retries one create_task six times at once.
    let rent = "Игра: оплатить аренду офиса";
    let ccid = uuid::Uuid::new_v4();
    let burst: Vec<_> = (0..6)
        .map(|_| {
            let bot = ApiClient::new(base.clone(), app.admin_token.clone());
            let body = create_task_command(&pid, rent, ccid);
            tokio::spawn(async move { bot.post_json("/v1/commands", body).await })
        })
        .collect();
    let mut event_ids = Vec::new();
    for call in burst {
        let res = call.await.unwrap().expect("retry must succeed");
        event_ids.push(res["event_id"].as_str().unwrap().to_owned());
    }
    eprintln!("TRACE step1 ccid={ccid} event_ids={event_ids:?}");
    event_ids.dedup();
    assert_eq!(event_ids.len(), 1, "one event_id for the burst");
    let rent_tasks = count(
        &app.pool,
        "SELECT COUNT(*) FROM tasks WHERE title = ?",
        rent,
    )
    .await;
    let listed = call_tool(
        &agent,
        "daruma_list",
        json!({ "project_id": pid, "status": "active" }),
    )
    .await;
    eprintln!("TRACE step1 tasks_with_title={rent_tasks} daruma_list={listed}");
    assert_eq!(rent_tasks, 1, "the burst must create exactly one task");

    // 2. A rule-blocked create does not consume its ccid.
    call_tool(
        &agent,
        "daruma_rule_create",
        json!({ "rule": {
            "rule_key": "game-task-decision",
            "title": "game-task-decision",
            "scope": { "kind": "project", "id": pid },
            "trigger": "task.created",
            "requirement": { "type": "decision_record" },
            "mode": "required",
            "message": "game-task-decision: зафиксируйте решение по игре",
            "override_allowed": false
        }}),
    )
    .await;
    let prizes = "Игра: закупить призы";
    let ccid = uuid::Uuid::new_v4();
    let bot = ApiClient::new(base.clone(), app.admin_token.clone());
    let err = bot
        .post_json("/v1/commands", create_task_command(&pid, prizes, ccid))
        .await
        .expect_err("rule gate must block the first attempt")
        .to_string();
    eprintln!("TRACE step2 blocked ccid={ccid} error={err}");
    assert!(err.contains("game-task-decision"), "{err}");
    let mut evidence = unblock_hints(&err)[0]["evidence"].clone();
    evidence["reason"] = json!("бюджет на призы утверждён на планёрке");
    let recorded = call_tool(
        &agent,
        "daruma_evidence_submit",
        json!({ "evidence": evidence }),
    )
    .await;
    eprintln!("TRACE step2 evidence_submit={recorded}");
    let retried = bot
        .post_json("/v1/commands", create_task_command(&pid, prizes, ccid))
        .await
        .expect("retry with the same ccid must run after the block");
    eprintln!("TRACE step2 retry event_id={}", retried["event_id"]);
    assert_eq!(retried["data"][0]["payload"]["type"], "task_created");
    assert_eq!(
        count(
            &app.pool,
            "SELECT COUNT(*) FROM tasks WHERE title = ?",
            prizes
        )
        .await,
        1
    );

    // 3. The agent's daruma_link retries collapse into one relation.
    let to = listed["items"][0]["id"].as_str().unwrap().to_owned();
    let from = retried["data"][0]["payload"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ccid = uuid::Uuid::new_v4();
    let links: Vec<_> = (0..6)
        .map(|_| {
            let agent = ApiClient::new(base.clone(), app.admin_token.clone());
            let args =
                json!({ "from": from, "to": to, "kind": "blocks", "client_command_id": ccid });
            tokio::spawn(async move { call_tool(&agent, "daruma_link", args).await })
        })
        .collect();
    let mut relation_ids = Vec::new();
    for call in links {
        let res = call.await.unwrap();
        relation_ids.push(res["data"]["relation_id"].clone());
    }
    eprintln!("TRACE step3 ccid={ccid} relation_ids={relation_ids:?}");
    relation_ids.dedup();
    assert_eq!(relation_ids.len(), 1, "one relation for the burst");
    let linked = count(
        &app.pool,
        "SELECT COUNT(*) FROM events WHERE kind = ?",
        "task.linked",
    )
    .await;
    let relations = call_tool(&agent, "daruma_relations", json!({ "task_id": from })).await;
    eprintln!("TRACE step3 task_linked_events={linked} daruma_relations={relations}");
    assert_eq!(linked, 1, "exactly one task.linked event");
    assert_eq!(relations["blocks"].as_array().map(Vec::len), Some(1));
}
