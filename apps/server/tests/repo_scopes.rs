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
use common::{json_get, json_post, spawn_server, test_app, TestAppBuilder};
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

async fn provision(addr: &std::net::SocketAddr, token: &str, scope_path: &str) -> Value {
    reqwest::Client::new()
        .post(format!("http://{addr}/v1/repo-scopes/provision"))
        .bearer_auth(token)
        .json(&json!({ "scope_path": scope_path }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn project_count(app: &Router, token: &str) -> usize {
    let (s, body) = json_get(app.clone(), token, "/v1/projects").await;
    assert_eq!(s, StatusCode::OK);
    body.as_array().map(|a| a.len()).unwrap_or(0)
}

#[tokio::test]
async fn provision_off_is_noop() {
    // Default builder → flag OFF: provision resolves nothing and creates nothing.
    let app = test_app().await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;

    let out = provision(&addr, &token, "/srv/fresh/repo").await;
    assert_eq!(out["provisioned"], false, "{out}");
    assert_eq!(out["project_id"], Value::Null, "{out}");
    assert_eq!(project_count(&app.router, &token).await, 0);

    let (_, scopes) = json_get(app.router.clone(), &token, "/v1/repo-scopes").await;
    assert_eq!(scopes, json!([]), "flag off must not bind: {scopes}");
}

#[tokio::test]
async fn provision_on_creates_and_is_idempotent() {
    let app = TestAppBuilder::default()
        .auto_provision_repo_project(true)
        .build()
        .await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;

    // First touch of a new repo → creates the project (title = basename) + binds.
    let first = provision(&addr, &token, "/srv/tenant/acme-api/").await;
    assert_eq!(first["provisioned"], true, "{first}");
    assert_eq!(first["scope_path"], "/srv/tenant/acme-api", "{first}");
    let project_id = first["project_id"].as_str().unwrap().to_string();
    assert_eq!(project_count(&app.router, &token).await, 1);

    let (_, projects) = json_get(app.router.clone(), &token, "/v1/projects").await;
    assert_eq!(projects[0]["title"], "acme-api", "title = basename: {projects}");

    // Second call for the same path is a no-op that returns the same project.
    let again = provision(&addr, &token, "/srv/tenant/acme-api").await;
    assert_eq!(again["provisioned"], false, "{again}");
    assert_eq!(again["project_id"].as_str().unwrap(), project_id);
    assert_eq!(project_count(&app.router, &token).await, 1, "no duplicate");
}

#[tokio::test]
async fn concurrent_first_touch_creates_one_project() {
    let app = TestAppBuilder::default()
        .auto_provision_repo_project(true)
        .build()
        .await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;

    // 8 concurrent first-touch calls for the same new repo.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let addr = addr;
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            provision(&addr, &token, "/srv/race/repo").await
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for h in handles {
        let out = h.await.unwrap();
        ids.insert(out["project_id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids.len(), 1, "all callers must see one project id: {ids:?}");
    assert_eq!(
        project_count(&app.router, &token).await,
        1,
        "the race must not create duplicate projects"
    );
}

#[tokio::test]
async fn mcp_list_auto_provisions_scoped_path() {
    // End-to-end: an unbound scope_path on a flag-on server provisions instead
    // of asking for project selection (done-criterion a).
    let app = TestAppBuilder::default()
        .auto_provision_repo_project(true)
        .build()
        .await;
    let token = app.admin_token.clone();
    let addr = spawn_server(&app).await;

    let listed = mcp_call(
        &addr,
        &token,
        "daruma_list",
        json!({ "status": "active", "scope_path": "/srv/auto/widget" }),
    )
    .await;
    assert!(
        listed.get("needs_project_selection").is_none(),
        "scope_path must auto-provision + resolve: {listed}"
    );

    // The binding now exists and points at a project titled after the basename.
    let (_, scopes) = json_get(app.router.clone(), &token, "/v1/repo-scopes").await;
    let arr = scopes.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{scopes}");
    assert_eq!(arr[0]["scope_path"], "/srv/auto/widget");
    assert_eq!(project_count(&app.router, &token).await, 1);
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
