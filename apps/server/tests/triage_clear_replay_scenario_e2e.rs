//! Task 01a153cd-ecc7 — corporate-game scenario against a real inline server
//! with production wiring (rule engine gate + plan-only intake).
//!
//! A triage lead accepts a materialized plan task over
//! `PATCH /v1/tasks/{id}/triage`, then withdraws the decision with
//! `{"triage_state": null}`. A replica (desktop client, second node) pulls the
//! event log over `GET /v1/events` and replays it into a fresh projection.
//! Expected: the server and the replica both show `triage_state = null`; the
//! `TaskUpdated` event on the wire carries the clear as `null` and reads back
//! as a clear, not as "unchanged" (which used to resurrect `accepted`).

use daruma_domain::TaskPatch;
use daruma_events::{Event, EventEnvelope};
use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use daruma_shared::TaskId;
use daruma_storage::{Db, TaskRepo};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, TestAppBuilder};

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_ne!(result["isError"], true, "tool {name} failed: {result}");
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn withdrawn_triage_decision_stays_cleared_on_a_replica() {
    let app = TestAppBuilder::default()
        .rule_engine_gate()
        .plan_only_intake(true)
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    let project = call_tool(
        &client,
        "daruma_project_create",
        json!({ "title": "Corporate game: triage withdrawal" }),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "Vendor onboarding", "project_id": pid },
            "tasks": [ { "title": "Collect the vendor security questionnaire" } ]
        }),
    )
    .await;
    let task_id = materialized["data"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|e| {
                let p = e.get("payload")?;
                (p.get("type")?.as_str()? == "task_created")
                    .then(|| p.get("task")?.get("id")?.as_str().map(str::to_owned))?
            })
        })
        .unwrap_or_else(|| panic!("task_created in {materialized}"));
    println!("TRACE step=materialize project={pid} task={task_id}");

    let accepted = client
        .patch_json(
            &format!("/v1/tasks/{task_id}/triage"),
            json!({ "triage_state": "accepted" }),
        )
        .await
        .unwrap();
    assert_eq!(accepted["triage_state"], "accepted", "{accepted}");
    let withdrawn = client
        .patch_json(
            &format!("/v1/tasks/{task_id}/triage"),
            json!({ "triage_state": null }),
        )
        .await
        .unwrap();
    assert_eq!(withdrawn["triage_state"], Value::Null, "{withdrawn}");
    println!(
        "TRACE step=triage accepted->null server_triage_state={} updated_event_seq={}",
        withdrawn["triage_state"], withdrawn["updated_event_seq"]
    );

    // Replica pulls the log the way the desktop client does.
    let raw = client
        .get_json("/v1/events?since=0&limit=1000")
        .await
        .unwrap();
    let envs: Vec<EventEnvelope> = serde_json::from_value(raw.clone()).unwrap();
    let id: TaskId = task_id.parse().unwrap();
    let triage_patches: Vec<(u64, Value, TaskPatch)> = raw
        .as_array()
        .unwrap()
        .iter()
        .zip(&envs)
        .filter_map(|(wire, env)| match &env.payload {
            Event::TaskUpdated { task_id, patch }
                if *task_id == id && wire["payload"]["patch"].get("triage_state").is_some() =>
            {
                Some((
                    env.seq,
                    wire["payload"]["patch"]["triage_state"].clone(),
                    patch.clone(),
                ))
            }
            _ => None,
        })
        .collect();
    for (seq, wire, patch) in &triage_patches {
        println!(
            "TRACE event seq={seq} type=task_updated wire_triage_state={wire} decoded={:?}",
            patch.triage_state
        );
    }
    assert_eq!(triage_patches.len(), 2, "accept + withdraw on the wire");
    let (_, wire, patch) = triage_patches.last().unwrap();
    assert_eq!(*wire, Value::Null);
    assert_eq!(
        patch.triage_state,
        Some(None),
        "null must decode as a clear"
    );

    let fresh = Db::memory().await.unwrap();
    fresh.migrate().await.unwrap();
    let replica = TaskRepo::new(fresh.pool().clone());
    for env in &envs {
        replica.apply_event(env).await.unwrap();
    }
    let replayed = replica.get(id).await.unwrap().expect("replayed task");
    let server = client
        .get_json(&format!("/v1/tasks/{task_id}"))
        .await
        .unwrap();
    println!(
        "TRACE step=replica replayed_events={} replica_triage_state={:?} server_triage_state={}",
        envs.len(),
        replayed.triage_state,
        server["triage_state"]
    );
    assert_eq!(server["triage_state"], Value::Null, "{server}");
    assert!(
        replayed.triage_state.is_none(),
        "replica resurrected {:?}",
        replayed.triage_state
    );
}
