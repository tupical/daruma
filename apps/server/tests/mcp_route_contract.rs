use std::sync::{Arc, Mutex};

use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::any,
    Router,
};
use daruma_mcp::{tool_definitions, tools::call_tool, ApiClient};
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tower::ServiceExt;

mod common;
use common::{json_post, spawn_server, test_app};

#[derive(Clone, Debug)]
struct Captured {
    method: Method,
    uri: String,
    body: Vec<u8>,
}

fn example(schema: &Value) -> Value {
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    {
        return value.clone();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(first) = schema
            .get(key)
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        {
            return example(first);
        }
    }
    match schema.get("type") {
        Some(Value::String(kind)) if kind == "object" => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let mut value = Map::new();
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                value.insert(
                    name.to_owned(),
                    properties
                        .and_then(|p| p.get(name))
                        .map(example)
                        .unwrap_or(Value::Null),
                );
            }
            Value::Object(value)
        }
        Some(Value::String(kind)) if kind == "array" => {
            json!([example(schema.get("items").unwrap_or(&Value::Null))])
        }
        Some(Value::String(kind)) if kind == "integer" => json!(1),
        Some(Value::String(kind)) if kind == "number" => json!(1.0),
        Some(Value::String(kind)) if kind == "boolean" => json!(false),
        Some(Value::Array(kinds)) if kinds.iter().any(|v| v == "string") => json!("x"),
        _ => match schema.get("format").and_then(Value::as_str) {
            Some("uuid") => json!("00000000-0000-0000-0000-000000000001"),
            Some("date-time") => json!("2026-01-01T00:00:00Z"),
            _ => json!("00000000-0000-0000-0000-000000000001"),
        },
    }
}

fn arguments(tool: &daruma_mcp::ToolDefinition) -> Value {
    let mut args = example(&tool.input_schema);
    match tool.name {
        "daruma_update" => args["title"] = json!("x"),
        "daruma_workspace_resolve" => args["scope_path"] = json!("/tmp/daruma-route-contract"),
        "daruma_reserve_files" => args["paths"] = json!(["src/lib.rs"]),
        _ => {}
    }
    args
}

fn event_object_id(response: &Value, event_type: &str, object: &str) -> String {
    response["data"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|event| {
            let payload = event.get("payload")?;
            (payload.get("type")?.as_str()? == event_type)
                .then(|| payload[object]["id"].as_str().unwrap().to_owned())
        })
        .unwrap_or_else(|| panic!("missing {event_type} in {response}"))
}

#[tokio::test]
async fn every_http_tool_matches_a_server_method_and_route() {
    let app = test_app().await;
    let route_probe = app
        .router
        .clone()
        .fallback(|| async { (StatusCode::IM_A_TEAPOT, "unmatched route").into_response() });
    let captured = Arc::new(Mutex::new(Vec::<Captured>::new()));
    let recorder = {
        let captured = captured.clone();
        Router::new().fallback(any(move |req: Request<Body>| {
            let captured = captured.clone();
            async move {
                let method = req.method().clone();
                let uri = req.uri().to_string();
                let body = to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                let response = if uri == "/v1/projects" {
                    json!([{
                        "id": "00000000-0000-0000-0000-000000000001",
                        "title": "x"
                    }])
                } else {
                    json!({})
                };
                captured
                    .lock()
                    .unwrap()
                    .push(Captured { method, uri, body });
                (StatusCode::OK, axum::Json(response))
            }
        }))
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let recorder_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, recorder).await.unwrap() });
    let recording_client = ApiClient::new(format!("http://{recorder_addr}"), "test-token");

    // These compatibility shims intentionally fail before HTTP and therefore
    // are outside the advertised HTTP-tool catalogue checked below.
    let excluded = ["daruma_create", "daruma_capture", "daruma_capture_batch"];
    let tools = tool_definitions();
    let mut covered = 0;
    for tool in &tools {
        captured.lock().unwrap().clear();
        let args = arguments(tool);
        let result = call_tool(&recording_client, tool.name, args).await;
        let mut requests = captured.lock().unwrap().clone();
        if tool.name == "daruma_handoff_respond" {
            captured.lock().unwrap().clear();
            call_tool(
                &recording_client,
                tool.name,
                json!({
                    "handoff_id": "00000000-0000-0000-0000-000000000001",
                    "decision": "reject",
                    "reason": "x"
                }),
            )
            .await
            .unwrap();
            requests.extend(captured.lock().unwrap().clone());
        }
        if tool.name == "daruma_project_delete" {
            let token = result.as_ref().unwrap()["confirm_token"].as_str().unwrap();
            captured.lock().unwrap().clear();
            call_tool(
                &recording_client,
                tool.name,
                json!({
                    "id": "00000000-0000-0000-0000-000000000001",
                    "confirm_token": token,
                    "confirm": "x"
                }),
            )
            .await
            .unwrap();
            requests.extend(captured.lock().unwrap().clone());
        }
        assert!(
            !requests.is_empty(),
            "{} never reached HTTP: {:?}",
            tool.name,
            result.err()
        );
        covered += 1;
        for request in requests {
            let replay = Request::builder()
                .method(request.method.clone())
                .uri(&request.uri)
                .header("authorization", format!("Bearer {}", app.admin_token))
                .header("content-type", "application/json")
                .body(Body::from(request.body))
                .unwrap();
            let status = route_probe.clone().oneshot(replay).await.unwrap().status();
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{} sent {} {} with the wrong method",
                tool.name,
                request.method,
                request.uri
            );
            assert_ne!(
                status,
                StatusCode::IM_A_TEAPOT,
                "{} sent {} {} to no server route",
                tool.name,
                request.method,
                request.uri
            );
        }
    }
    assert_eq!(covered, tools.len());
    for tool in excluded {
        captured.lock().unwrap().clear();
        let result = call_tool(&recording_client, tool, json!({})).await;
        assert!(result.is_err(), "{tool} must remain a local bridge error");
        assert!(
            captured.lock().unwrap().is_empty(),
            "{tool} unexpectedly used HTTP"
        );
    }

    // Prove the three repaired dispatches succeed through call_tool against
    // the real server, not merely against the recorder above.
    let server_addr = spawn_server(&app).await;
    let client = ApiClient::new(format!("http://{server_addr}"), app.admin_token.clone());
    let (status, project) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"MCP route contract"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let project_id = event_object_id(&project, "project_created", "project");
    let (status, plan) = json_post(
        app.router.clone(),
        &app.admin_token,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"create_plan","plan":{{"project_id":"{project_id}","title":"Contract","owner":{{"kind":"user"}}}}}}}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    let plan_id = event_object_id(&plan, "plan_created", "plan");
    let (status, active) = json_post(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{plan_id}/status"),
        r#"{"status":"active"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{active}");
    let run = call_tool(
        &client,
        "daruma_run_start",
        json!({"plan_id": plan_id, "agent_id": app.admin_agent_id.as_uuid().to_string()}),
    )
    .await
    .unwrap();
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    call_tool(
        &client,
        "daruma_signal_send",
        json!({"run_id": run_id, "kind": {"kind": "stop", "reason": "test"}}),
    )
    .await
    .unwrap();
    call_tool(
        &client,
        "daruma_signal_respond",
        json!({"run_id": run_id, "choice": "continue"}),
    )
    .await
    .unwrap();
    let session = call_tool(
        &client,
        "daruma_session_start",
        json!({"agent_id": app.admin_agent_id.as_uuid().to_string()}),
    )
    .await
    .unwrap();
    let session_id = session["data"]["id"].as_str().unwrap().to_owned();
    call_tool(
        &client,
        "daruma_session_set_plan",
        json!({"id": session_id, "steps": [{"content": "verify routes", "status": "pending"}]}),
    )
    .await
    .unwrap();
}
