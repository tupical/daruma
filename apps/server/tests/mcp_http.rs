//! HTTP MCP endpoint smoke tests.

use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_mcp::{tools::call_tool, ApiClient};
use serde_json::json;

mod common;
use common::{mint_pat, spawn_server, test_app};

async fn workspace_agent_id(
    client: &reqwest::Client,
    addr: &std::net::SocketAddr,
    token: &str,
    request_id: u64,
) -> String {
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": "daruma_workspace_info", "arguments": {}}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str::<serde_json::Value>(text).unwrap()["mcp_agent_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn http_mcp_dispatches_tool_calls() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(&app.admin_token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "daruma_healthz",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let healthz: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(healthz["status"], "ok");
}

#[tokio::test]
async fn hosted_mcp_identity_is_stable_and_bound_to_the_authenticated_principal() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::new();
    let caps: Capabilities = [Capability::TaskRead].into();
    let (other_token, other_agent_id) = mint_pat(&app.auth_store(), caps, ProjectFilter::All).await;

    let first = workspace_agent_id(&client, &addr, &app.admin_token, 1).await;
    let second = workspace_agent_id(&client, &addr, &app.admin_token, 2).await;
    let other = workspace_agent_id(&client, &addr, &other_token, 3).await;

    assert_eq!(first, app.admin_agent_id.as_uuid().to_string());
    assert_eq!(
        second, first,
        "same bearer principal must keep one MCP identity"
    );
    assert_eq!(other, other_agent_id.as_uuid().to_string());
    assert_ne!(other, first);
}

#[tokio::test]
async fn stdio_workspace_identity_can_claim_and_release_its_own_task() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .bearer_auth(&app.admin_token)
        .json(&json!({"command": {"type": "create_task", "task": {"title": "stdio claim"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| event["payload"]["task"]["id"].as_str())
        .unwrap()
        .to_string();
    let api = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());
    let workspace = call_tool(&api, "daruma_workspace_info", json!({}))
        .await
        .unwrap();
    let agent_id = workspace["mcp_agent_id"].as_str().unwrap();

    call_tool(
        &api,
        "daruma_claim",
        json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": 60}),
    )
    .await
    .unwrap();
    call_tool(
        &api,
        "daruma_release",
        json!({"agent_id": agent_id, "task_id": task_id}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn stdio_dedup_refreshes_on_each_claim_generation_and_release() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{addr}/v1/commands"))
        .bearer_auth(&app.admin_token)
        .json(&json!({"command": {"type": "create_task", "task": {"title": "dedup claim"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = response["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|event| event["payload"]["task"]["id"].as_str())
        .unwrap()
        .to_string();
    let agent_id = app.admin_agent_id.as_uuid().to_string();
    let api = ApiClient::new(format!("http://{addr}"), app.admin_token.clone())
        .with_agent_id(app.admin_agent_id);
    let get = || call_tool(&api, "daruma_get", json!({"id": task_id, "dedup": true}));

    let first = get().await.unwrap();
    assert_eq!(first["current_claim"], serde_json::Value::Null);
    assert_eq!(get().await.unwrap()["unchanged"], true);

    let first_claim = call_tool(
        &api,
        "daruma_claim",
        json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": 60}),
    )
    .await
    .unwrap();
    let first_generation = first_claim["data"]["claim_id"].clone();
    let after_claim = get().await.unwrap();
    assert_ne!(after_claim["unchanged"], true);
    assert_eq!(after_claim["current_claim"]["claim_id"], first_generation);
    assert_eq!(get().await.unwrap()["unchanged"], true);

    let refreshed = call_tool(
        &api,
        "daruma_claim",
        json!({"agent_id": agent_id, "task_id": task_id, "ttl_secs": 120}),
    )
    .await
    .unwrap();
    let refreshed_generation = refreshed["data"]["claim_id"].clone();
    assert_ne!(refreshed_generation, first_generation);
    let after_refresh = get().await.unwrap();
    assert_ne!(after_refresh["unchanged"], true);
    assert_eq!(
        after_refresh["current_claim"]["claim_id"],
        refreshed_generation
    );

    call_tool(
        &api,
        "daruma_release",
        json!({"agent_id": agent_id, "task_id": task_id}),
    )
    .await
    .unwrap();
    let after_release = get().await.unwrap();
    assert_ne!(after_release["unchanged"], true);
    assert_eq!(after_release["current_claim"], serde_json::Value::Null);
}

#[tokio::test]
async fn http_mcp_profile_query_param_selects_surface() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::new();

    // Default (no query param): advanced tool is hidden and not callable.
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "daruma_history_latest", "arguments": {} }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["result"]["isError"], true, "got: {body}");
    let msg = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(msg.contains("not available"), "got: {body}");

    // ?profile=full: the same call dispatches.
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp?profile=full"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "daruma_history_latest", "arguments": { "limit": 1 } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        body["error"].is_null() && body["result"]["isError"] != true,
        "full profile must dispatch: {body}"
    );

    // Unknown profile → validation error.
    let resp = client
        .post(format!("http://{addr}/v1/mcp?profile=bogus"))
        .bearer_auth(&app.admin_token)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

async fn hosted_tool(
    client: &reqwest::Client,
    addr: &std::net::SocketAddr,
    token: &str,
    request_id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let body: serde_json::Value = client
        .post(format!("http://{addr}/v1/mcp?profile=full"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: {body}"));
    serde_json::from_str(text).unwrap_or_else(|err| panic!("{name}: {err}: {text}"))
}

fn listed_session_ids(listed: &serde_json::Value) -> Vec<String> {
    listed["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("session list: {listed}"))
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn stdio_session_defaults_to_workspace_info_agent_id() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let api = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());
    let workspace = call_tool(&api, "daruma_workspace_info", json!({}))
        .await
        .unwrap();
    let agent_id = workspace["mcp_agent_id"].as_str().unwrap().to_string();
    assert_ne!(
        agent_id,
        api.agent_id(),
        "stdio process id must differ from the token-bound id for this test to bite"
    );

    let started = call_tool(&api, "daruma_session_start", json!({}))
        .await
        .unwrap();
    assert_eq!(started["data"]["agent_id"], agent_id.as_str());
    let session_id = started["data"]["id"].as_str().unwrap().to_string();

    let by_default = call_tool(&api, "daruma_session_list", json!({}))
        .await
        .unwrap();
    assert!(listed_session_ids(&by_default).contains(&session_id));
    let by_workspace_id = call_tool(&api, "daruma_session_list", json!({"agent_id": agent_id}))
        .await
        .unwrap();
    assert!(listed_session_ids(&by_workspace_id).contains(&session_id));

    let explicit = call_tool(
        &api,
        "daruma_session_start",
        json!({"agent_id": api.agent_id()}),
    )
    .await
    .unwrap();
    assert_eq!(explicit["data"]["agent_id"], api.agent_id());
}

#[tokio::test]
async fn hosted_session_defaults_to_workspace_info_agent_id() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::new();
    let agent_id = workspace_agent_id(&client, &addr, &app.admin_token, 1).await;

    let started = hosted_tool(
        &client,
        &addr,
        &app.admin_token,
        2,
        "daruma_session_start",
        json!({}),
    )
    .await;
    assert_eq!(started["data"]["agent_id"], agent_id.as_str());
    let session_id = started["data"]["id"].as_str().unwrap().to_string();

    let listed = hosted_tool(
        &client,
        &addr,
        &app.admin_token,
        3,
        "daruma_session_list",
        json!({}),
    )
    .await;
    assert!(listed_session_ids(&listed).contains(&session_id));
}

async fn hosted_tool_as_client(
    addr: &std::net::SocketAddr,
    token: &str,
    client_name: Option<&str>,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let mut request = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp?profile=full"))
        .bearer_auth(token);
    if let Some(client_name) = client_name {
        request = request.header("X-Daruma-Client", client_name);
    }
    let body: serde_json::Value = request
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{name}: {body}"));
    serde_json::from_str(text).unwrap_or_else(|err| panic!("{name}: {err}: {text}"))
}

/// T1 (01a17eb7-2039): the hosting gateway names the MCP client in
/// `X-Daruma-Client`; TaskCreated / TaskStatusChanged / CommentAdded written
/// through that MCP call carry it in `actor.client`. Without the header the
/// actor keeps its legacy shape (no `client`).
#[tokio::test]
async fn hosted_mcp_attributes_task_events_to_the_named_client() {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    let token = app.admin_token.clone();
    let codex = Some("codex-mcp-client");

    let project = hosted_tool_as_client(
        &addr,
        &token,
        codex,
        "daruma_project_create",
        json!({"title": "Client attribution"}),
    )
    .await;
    let pid = project["project_id"].as_str().unwrap().to_owned();
    let materialized = hosted_tool_as_client(
        &addr,
        &token,
        codex,
        "daruma_plan_materialize",
        json!({
            "plan": {"title": "Client plan", "project_id": pid},
            "tasks": [{"title": "Attributed task"}]
        }),
    )
    .await;
    let task_id = materialized["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            (p.get("type")?.as_str()? == "task_created")
                .then(|| p["task"]["id"].as_str().map(str::to_owned))?
        })
        .unwrap_or_else(|| panic!("task_created in {materialized}"));
    hosted_tool_as_client(
        &addr,
        &token,
        codex,
        "daruma_set_status",
        json!({"id": task_id, "status": "in_progress"}),
    )
    .await;
    hosted_tool_as_client(
        &addr,
        &token,
        codex,
        "daruma_comment",
        json!({"task_id": task_id, "body": "attributed comment"}),
    )
    .await;
    // Same principal, no header: legacy shape, no client.
    hosted_tool_as_client(
        &addr,
        &token,
        None,
        "daruma_comment",
        json!({"task_id": task_id, "body": "anonymous client comment"}),
    )
    .await;

    let events = app.state.store.load_since(0, 1000).await.unwrap();
    let clients = |kind: &str| -> Vec<Option<String>> {
        events
            .iter()
            .filter(|e| e.kind() == kind)
            .map(|e| e.actor.client().map(str::to_owned))
            .collect()
    };
    let codex = Some("codex-mcp-client".to_string());
    assert_eq!(clients("task_created"), vec![codex.clone()]);
    assert_eq!(clients("task_status_changed"), vec![codex.clone()]);
    assert_eq!(clients("comment_added"), vec![codex, None]);
}

#[tokio::test]
async fn project_snapshot_pages_corporate_game_events_over_mcp() {
    use daruma_domain::Actor;
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::ProjectId;
    use serde_json::Value;

    let app = common::TestAppBuilder::default()
        .plan_only_intake(true)
        .rule_engine_gate()
        .build()
        .await;
    let addr = spawn_server(&app).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{addr}/v1/mcp?profile=full");
    let call = |token: String, name: &'static str, args: Value| {
        let client = &client;
        let url = &url;
        async move {
            let response = client
                .post(url)
                .bearer_auth(token)
                .json(&json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": name, "arguments": args}
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let trace = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let body: Value = response.json().await.unwrap();
            assert!(body["error"].is_null(), "{body}");
            let result = &body["result"];
            let text = result["content"][0]["text"].as_str().unwrap();
            if result["isError"] == true {
                println!("TRACE {name} request_id={trace} error={text}");
                return Err(text.to_owned());
            }
            println!("TRACE {name} request_id={trace} ok");
            Ok(serde_json::from_str::<Value>(text).unwrap())
        }
    };
    let admin = app.admin_token.clone();
    let create = |title| {
        call(
            admin.clone(),
            "daruma_project_create",
            json!({"title": title}),
        )
    };
    let other: ProjectId =
        serde_json::from_value(create("Other project").await.unwrap()["project_id"].clone())
            .unwrap();
    let project: ProjectId =
        serde_json::from_value(create("corporate-game").await.unwrap()["project_id"].clone())
            .unwrap();
    let store = app.event_store();
    let since = store.latest_seq().await.unwrap();
    let unrelated = store
        .append_batch(
            (0..1001)
                .map(|_| {
                    EventEnvelope::new(
                        Actor::user(),
                        Event::ProjectUpdated {
                            project_id: other,
                            title: None,
                            description: None,
                        },
                    )
                })
                .collect(),
        )
        .await
        .unwrap();
    let plan = call(
        admin.clone(),
        "daruma_plan_materialize",
        json!({
            "plan": {"title": "Delivery", "project_id": project},
            "tasks": [{"title": "Verify the corporate-game release"}]
        }),
    )
    .await
    .unwrap();
    fn event<'a>(data: &'a Value, kind: &str) -> &'a Value {
        data["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["payload"]["type"] == kind)
            .unwrap()
    }
    let task_event = event(&plan, "task_created");
    let task = &task_event["payload"]["task"]["id"];
    store
        .append(EventEnvelope::new(
            Actor::user(),
            Event::ProjectUpdated {
                project_id: other,
                title: None,
                description: None,
            },
        ))
        .await
        .unwrap();
    let status = call(
        admin.clone(),
        "daruma_set_status",
        json!({
            "id": task, "status": "in_progress"
        }),
    )
    .await
    .unwrap();
    let before = store.latest_seq().await.unwrap();
    let snapshot = "daruma_subscribe_project";
    let history = "daruma_events_since";
    let args = json!({"project_id": project, "since_seq": since, "limit": 1});
    let (reader, _) = mint_pat(
        &app.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::All,
    )
    .await;
    let expected = [
        event(&plan, "plan_created"),
        task_event,
        event(&plan, "plan_task_added"),
        event(&status, "task_status_changed"),
    ];
    let mut cursor = json!(since);
    let mut selector = json!(project);
    let mut sequences = Vec::new();
    for expected in expected {
        let page = call(
            reader.clone(),
            snapshot,
            json!({
                "project_id": selector, "since_seq": cursor, "limit": 1
            }),
        )
        .await
        .unwrap();
        assert_eq!(page.as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page[0]["payload"]["type"], expected["payload"]["type"]);
        assert_eq!(page[0]["seq"], expected["seq"]);
        cursor = page[0]["seq"].clone();
        sequences.push(cursor.as_u64().unwrap());
        selector = json!(project.to_string());
    }
    let tail = call(
        reader.clone(),
        snapshot,
        json!({
            "project_id": project, "since_seq": cursor
        }),
    )
    .await
    .unwrap();
    assert_eq!(tail, json!([]));

    let global = call(reader.clone(), history, json!({"seq": since, "limit": 1}))
        .await
        .unwrap();
    assert_eq!(global.as_array().unwrap().len(), 1);
    assert_eq!(global[0]["seq"], unrelated[0].seq);
    assert_eq!(global[0]["payload"]["project_id"], json!(other));
    for (selector, error) in [
        (json!({}), "project_id"),
        (json!({"project_id": 123}), "project_id"),
        (json!({"project_id": "all"}), "invalid project_id"),
        (json!({"project_id": ProjectId::new()}), "404"),
    ] {
        let actual = call(admin.clone(), snapshot, selector).await.unwrap_err();
        assert!(actual.contains(error), "{actual}");
    }

    let (scoped, _) = mint_pat(
        &app.auth_store(),
        [Capability::TaskRead].into(),
        ProjectFilter::Only {
            projects: vec![project],
        },
    )
    .await;
    let (no_task_read, _) = mint_pat(
        &app.auth_store(),
        [Capability::ProjectRead].into(),
        ProjectFilter::All,
    )
    .await;
    for token in [scoped, no_task_read] {
        assert!(call(token.clone(), snapshot, args.clone())
            .await
            .unwrap_err()
            .contains("403"));
        assert!(call(token, history, json!({"seq": since, "limit": 1}))
            .await
            .unwrap_err()
            .contains("403"));
    }
    assert_eq!(store.latest_seq().await.unwrap(), before);
    println!(
        "TRACE corporate-game project={project} event_seqs={sequences:?} global_seq={} head={before}",
        global[0]["seq"]
    );

    call(
        admin.clone(),
        "daruma_rule_create",
        json!({"rule": {
            "rule_key": "game-task-decision",
            "title": "Record the game decision",
            "scope": {"kind": "project", "id": project},
            "trigger": "task.created",
            "requirement": {"type": "decision_record"},
            "mode": "recommendation",
            "message": "Record a decision before preparing the next game",
            "override_allowed": false
        }}),
    )
    .await
    .unwrap();
    let before_warning = store.latest_seq().await.unwrap();
    let planned = call(
        admin.clone(),
        "daruma_plan_materialize",
        json!({
            "plan": {"title": "Next corporate game", "project_id": project},
            "tasks": [{"title": "Book the venue"}]
        }),
    )
    .await
    .unwrap();
    let moved_task = &event(&planned, "task_created")["payload"]["task"]["id"];
    call(
        admin.clone(),
        "daruma_amend_plan_task",
        json!({
            "plan_id": event(&planned, "plan_created")["payload"]["plan"]["id"],
            "task_id": moved_task,
            "patch": {"project_id": other}
        }),
    )
    .await
    .unwrap();
    let moved = call(admin, "daruma_get", json!({"id": moved_task}))
        .await
        .unwrap();
    assert_eq!(moved["project_id"], json!(other), "{moved}");
    let warning = store
        .load_since(before_warning, 20)
        .await
        .unwrap()
        .into_iter()
        .find(|env| env.payload.kind() == "rule_fired")
        .expect("the project rule fired during materialize");
    let Event::RuleFired {
        project_id, task_id, ..
    } = &warning.payload
    else {
        unreachable!()
    };
    assert_eq!(*project_id, Some(project));
    assert_eq!(json!(task_id), *moved_task);

    let before_graph = store.latest_seq().await.unwrap();
    let graph = store
        .append(EventEnvelope::new(
            Actor::user(),
            Event::GraphNodeUpserted {
                node_id: "next-game-brief".into(),
                kind: "PlanBrief".into(),
                source_id: "next-game-brief".into(),
                project_id: Some(project.as_uuid().to_string()),
                title: "Next game brief".into(),
                text: String::new(),
                metadata_json: json!({}),
                edges: Vec::new(),
                at: daruma_shared::time::now(),
            },
        ))
        .await
        .unwrap();
    let page = |project_id: ProjectId, since_seq| {
        call(
            reader.clone(),
            snapshot,
            json!({"project_id": project_id, "since_seq": since_seq, "limit": 1}),
        )
    };
    let head = store.latest_seq().await.unwrap();
    let warning_in_original = page(project, warning.seq - 1).await.unwrap();
    let warning_in_neighbor = page(other, warning.seq - 1).await.unwrap();
    let graph_in_original = page(project, before_graph).await.unwrap();
    let graph_in_neighbor = page(other, before_graph).await.unwrap();
    println!(
        "TRACE corporate-game original={project} neighbor={other} moved_task={moved_task} \
         warning_id={} warning_seq={} graph_id={} graph_seq={} pages={:?} head={head}",
        warning.id,
        warning.seq,
        graph.id,
        graph.seq,
        [
            &warning_in_original[0]["seq"],
            &warning_in_neighbor[0]["seq"],
            &graph_in_original[0]["seq"],
            &graph_in_neighbor[0]["seq"],
        ],
    );
    assert_eq!(warning_in_original[0]["id"], json!(warning.id));
    assert!(warning_in_neighbor[0]["seq"].as_u64().unwrap() > warning.seq);
    assert_eq!(graph_in_original[0]["id"], json!(graph.id));
    assert_eq!(graph_in_neighbor, json!([]));
    assert_eq!(store.latest_seq().await.unwrap(), head);
}

#[tokio::test]
async fn project_snapshot_rejects_a_legacy_workspace_log() {
    use axum::{routing::get, Json, Router};
    use daruma_shared::ProjectId;

    let app = test_app().await;
    let legacy = common::TestApp {
        router: Router::new().route("/v1/events", get(|| async { Json(json!([{"seq": 1}])) })),
        ..app
    };
    let addr = spawn_server(&legacy).await;
    let api = ApiClient::new(format!("http://{addr}"), legacy.admin_token.clone());
    let result = call_tool(
        &api,
        "daruma_subscribe_project",
        json!({"project_id": ProjectId::new()}),
    )
    .await;
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("server did not provide a project snapshot"),
        "{error}"
    );
}
