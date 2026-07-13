//! Repo scope bindings end-to-end (migration 0046): REST surface
//! (`GET/PUT /v1/repo-scopes`) plus server-mode MCP resolution — a hosted
//! session with no local workspace key resolves `scope_path` and
//! `daruma_project_use` against the server-side table.

mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use common::{json_post, spawn_server, test_app};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn json_method(
    app: Router,
    method: Method,
    token: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_owned()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn create_project(app: &Router, token: &str, title: &str) -> String {
    let (s, ev) = json_post(
        app.clone(),
        token,
        "/v1/commands",
        &format!(r#"{{"command":{{"type":"create_project","title":"{title}"}}}}"#),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create_project failed: {ev}");
    ev["data"]
        .as_array()
        .expect("data array")
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "project_created" {
                p.get("project")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("project id")
}

#[tokio::test]
async fn put_get_and_remove_bindings() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let project_id = create_project(&app.router, &token, "Repo Scopes").await;

    // Unknown project is rejected.
    let (s, _) = json_method(
        app.router.clone(),
        Method::PUT,
        &token,
        "/v1/repo-scopes",
        Some(r#"{"scope_path":"/tmp/rs-test/app","project_id":"019e0000-0000-7000-8000-000000000000"}"#),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Empty scope_path is rejected.
    let (s, _) = json_method(
        app.router.clone(),
        Method::PUT,
        &token,
        "/v1/repo-scopes",
        Some(&format!(
            r#"{{"scope_path":"  ","project_id":"{project_id}"}}"#
        )),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Upsert (trailing slash trimmed) then list.
    let (s, body) = json_method(
        app.router.clone(),
        Method::PUT,
        &token,
        "/v1/repo-scopes",
        Some(&format!(
            r#"{{"scope_path":"/tmp/rs-test/app/","project_id":"{project_id}"}}"#
        )),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["scope_path"], "/tmp/rs-test/app");

    let (s, body) = json_method(
        app.router.clone(),
        Method::GET,
        &token,
        "/v1/repo-scopes",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        json!([{ "scope_path": "/tmp/rs-test/app", "project_id": project_id }])
    );

    // project_id null removes the binding.
    let (s, _) = json_method(
        app.router.clone(),
        Method::PUT,
        &token,
        "/v1/repo-scopes",
        Some(r#"{"scope_path":"/tmp/rs-test/app","project_id":null}"#),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, body) = json_method(
        app.router.clone(),
        Method::GET,
        &token,
        "/v1/repo-scopes",
        None,
    )
    .await;
    assert_eq!(body, json!([]));
}

async fn mcp_call(addr: &std::net::SocketAddr, token: &str, name: &str, args: Value) -> Value {
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["error"].is_null(), "{name} errored: {body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn server_mode_mcp_resolves_scopes_from_table() {
    let app = test_app().await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;
    let project_id = create_project(&app.router, &token, "Hosted Scope").await;

    // Bind via the MCP tool itself — server mode has no CWD, so an
    // absolute scope_path is required and sufficient.
    let bound = mcp_call(
        &addr,
        &token,
        "daruma_project_use",
        json!({ "project_id": project_id, "scope_path": "/srv/tenant/repo" }),
    )
    .await;
    assert_eq!(bound["scope"], "/srv/tenant/repo");

    // workspace_info now reports the binding even without local state.
    let info = mcp_call(&addr, &token, "daruma_workspace_info", json!({})).await;
    let scopes = info["scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 1, "{info}");
    assert_eq!(scopes[0]["scope"], "/srv/tenant/repo");
    assert_eq!(scopes[0]["name"], "repo");
    assert_eq!(scopes[0]["project_id"], project_id.as_str());

    // daruma_list scoped by path inside the bound repo resolves the project
    // instead of asking for project selection.
    let listed = mcp_call(
        &addr,
        &token,
        "daruma_list",
        json!({ "status": "active", "scope_path": "/srv/tenant/repo/crates/api" }),
    )
    .await;
    assert!(
        listed.get("needs_project_selection").is_none(),
        "scope_path must resolve the project: {listed}"
    );

    // A path outside any binding still fails loudly.
    let body: Value = reqwest::Client::new()
        .post(format!("http://{addr}/v1/mcp"))
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "daruma_list",
                        "arguments": { "status": "active", "scope_path": "/srv/other" } }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("no daruma scope configured"),
        "got: {body}"
    );
}
