//! MCP tool integration tests for document tools (PR1 §9).
//!
//! Covers the spec scenarios end-to-end through the MCP dispatcher:
//!   1. `daruma_project_create` does NOT auto-seed documents (execution core
//!      starts a project bare; narrative docs are a product concern).
//!   2. `daruma_doc_append` → `daruma_doc_get` reflects the appended chunk.
//!   3. `daruma_doc_create(kind=Interview)` allows duplicate kinds.
//!   4. `daruma_doc_list(project_id, kind=HumanLog)` filters by kind.
//!   5. `daruma_doc_archive` hides the doc from `daruma_doc_list`
//!      until `include_archived=true`.

use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{spawn_server, test_app};

async fn spawn_daruma_inline() -> (std::net::SocketAddr, String) {
    let app = test_app().await;
    let addr = spawn_server(&app).await;
    (addr, app.admin_token)
}

fn req(method: &str, params: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params: Some(params),
    }
}

async fn call_tool(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let resp = dispatch_request(
        client,
        req(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        ),
    )
    .await
    .unwrap();
    assert!(resp.error.is_none(), "tool {name} failed: {:?}", resp.error);
    let text = resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    serde_json::from_str(&text).unwrap()
}

async fn create_project_via_mcp(client: &ApiClient, title: &str) -> String {
    let resp = call_tool(client, "daruma_project_create", json!({ "title": title })).await;
    resp["project_id"]
        .as_str()
        .expect("project_id must be a string in response")
        .to_owned()
}

/// Create a task under `pid` via the plan-only intake (`daruma_plan_materialize`
/// — ADR-0007, direct task creation is not exposed) and return its id. The
/// document-task anchor barrier (task 019f6ad2) requires every document to be
/// anchored to a task at creation, so tests that need a document seed one of
/// these first.
async fn create_task_via_mcp(client: &ApiClient, pid: &str, title: &str) -> String {
    let resp = call_tool(
        client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": format!("{title} plan"), "project_id": pid },
            "tasks": [ { "title": title } ]
        }),
    )
    .await;
    resp["data"]
        .as_array()
        .expect("event envelopes")
        .iter()
        .find_map(|e| e["payload"]["task"]["id"].as_str())
        .expect("task id in materialize response")
        .to_owned()
}

/// Create a document of `kind` under `pid`, anchored to a freshly created
/// task, via MCP and return its id. The core no longer auto-seeds docs, so
/// tests that need a document seed it explicitly through the same public
/// tools agents use.
async fn create_doc_via_mcp(client: &ApiClient, pid: &str, kind: &str, title: &str) -> String {
    let task_id = create_task_via_mcp(client, pid, "doc anchor").await;
    create_doc_via_mcp_for_task(client, pid, kind, title, &task_id).await
}

/// Create a document of `kind` under `pid`, anchored to `task_id`, via MCP
/// and return its id.
async fn create_doc_via_mcp_for_task(
    client: &ApiClient,
    pid: &str,
    kind: &str,
    title: &str,
    task_id: &str,
) -> String {
    let resp = call_tool(
        client,
        "daruma_doc_create",
        json!({ "project_id": pid, "kind": kind, "title": title, "task_id": task_id }),
    )
    .await;
    assert_eq!(resp["success"], true, "doc_create must succeed: {resp}");
    resp["document_id"]
        .as_str()
        .expect("document_id must be a string in response")
        .to_owned()
}

/// Project creation does NOT auto-seed documents: a freshly created project
/// has an empty document list. Narrative Interview / Human Log docs are a
/// product concern and are created explicitly, not by the execution core.
#[tokio::test]
async fn project_create_does_not_seed_documents() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;

    let docs = call_tool(&client, "daruma_doc_list", json!({ "project_id": pid })).await;
    let arr = docs.as_array().expect("doc list must be array");
    assert!(
        arr.is_empty(),
        "fresh project has no auto-seeded docs: {arr:?}"
    );
}

/// Appending a chunk via `daruma_doc_append` must show up in
/// `daruma_doc_get` immediately.
#[tokio::test]
async fn doc_append_reflected_in_doc_get() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;
    let interview_id = create_doc_via_mcp(&client, &pid, "interview", "Interview").await;

    let appended_snippet = "appended-chunk-marker";
    let append_resp = call_tool(
        &client,
        "daruma_doc_append",
        json!({ "document_id": interview_id, "content": appended_snippet }),
    )
    .await;
    assert_eq!(
        append_resp["success"], true,
        "append must succeed: {append_resp}"
    );

    let got = call_tool(
        &client,
        "daruma_doc_get",
        json!({ "document_id": interview_id }),
    )
    .await;
    let body = got["document"]["content"].as_str().unwrap();
    assert!(
        body.contains(appended_snippet),
        "appended snippet must be in body: {body:?}"
    );
}

/// `daruma_doc_create(kind=interview)` must succeed even when an Interview
/// document already exists — kind is not unique per project.
#[tokio::test]
async fn doc_create_allows_duplicate_kind() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;
    let task_id = create_task_via_mcp(&client, &pid, "anchor").await;

    // First Interview (created explicitly — nothing is auto-seeded).
    create_doc_via_mcp_for_task(&client, &pid, "interview", "First Interview", &task_id).await;

    let resp = call_tool(
        &client,
        "daruma_doc_create",
        json!({
            "project_id": pid,
            "kind": "interview",
            "title": "Second Interview",
            "task_id": task_id,
        }),
    )
    .await;
    assert_eq!(
        resp["success"], true,
        "second Interview create must succeed: {resp}"
    );
    assert!(
        resp["document_id"].is_string(),
        "doc_create must surface document_id for agents: {resp}"
    );

    let docs = call_tool(
        &client,
        "daruma_doc_list",
        json!({ "project_id": pid, "kind": "interview" }),
    )
    .await;
    let arr = docs.as_array().expect("doc list array");
    assert_eq!(arr.len(), 2, "two Interview docs now exist: {arr:?}");
}

/// `daruma_doc_list` with `kind` filter must return only docs of that kind.
#[tokio::test]
async fn doc_list_filters_by_kind() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;
    // Seed one doc of each kind explicitly.
    create_doc_via_mcp(&client, &pid, "interview", "Interview").await;
    create_doc_via_mcp(&client, &pid, "human_log", "Human Log").await;

    let only_log = call_tool(
        &client,
        "daruma_doc_list",
        json!({ "project_id": pid, "kind": "human_log" }),
    )
    .await;
    let arr = only_log.as_array().expect("doc list array");
    assert_eq!(arr.len(), 1, "exactly one HumanLog: {arr:?}");
    assert_eq!(arr[0]["kind"], "human_log");
}

/// `daruma_doc_archive` must remove the doc from the default `doc_list`
/// view, but the doc must reappear when `include_archived=true`.
#[tokio::test]
async fn doc_archive_hides_from_default_list() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;
    // Seed one doc of each kind; archive the Interview, expect the HumanLog
    // to remain in the default view.
    let interview_id = create_doc_via_mcp(&client, &pid, "interview", "Interview").await;
    create_doc_via_mcp(&client, &pid, "human_log", "Human Log").await;

    let archive_resp = call_tool(
        &client,
        "daruma_doc_archive",
        json!({ "document_id": interview_id }),
    )
    .await;
    assert_eq!(
        archive_resp["success"], true,
        "archive must succeed: {archive_resp}"
    );

    // Default list (include_archived=false) hides the doc.
    let default_list = call_tool(&client, "daruma_doc_list", json!({ "project_id": pid })).await;
    let default_arr = default_list.as_array().expect("default list array");
    assert!(
        default_arr.iter().all(|d| d["id"] != interview_id),
        "archived doc must be hidden from default list: {default_arr:?}"
    );
    assert_eq!(
        default_arr.len(),
        1,
        "only HumanLog remains in default view"
    );

    // include_archived=true brings it back.
    let with_archived = call_tool(
        &client,
        "daruma_doc_list",
        json!({ "project_id": pid, "include_archived": true }),
    )
    .await;
    let with_arr = with_archived.as_array().expect("with-archived array");
    let revived = with_arr
        .iter()
        .find(|d| d["id"] == interview_id)
        .expect("archived doc visible with include_archived=true");
    assert!(
        !revived["archived_at"].is_null(),
        "archived_at must be set: {revived:?}"
    );
}

/// `daruma_doc_create` without `task_id` is rejected by the document-task
/// anchor barrier (task 019f6ad2) — a document is born anchored to a task
/// or not at all.
#[tokio::test]
async fn doc_create_without_task_id_is_rejected() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;

    let resp = dispatch_request(
        &client,
        req(
            "tools/call",
            json!({
                "name": "daruma_doc_create",
                "arguments": { "project_id": pid, "kind": "interview", "title": "Free-floating" },
            }),
        ),
    )
    .await
    .unwrap();
    assert!(
        resp.error.is_some(),
        "doc_create without task_id must fail: {resp:?}"
    );
    let msg = resp.error.unwrap().message;
    assert!(
        msg.contains("task_id") || msg.contains("anchored"),
        "error should explain the missing anchor: {msg}"
    );
}

/// All protocol-level tests drive the complete catalogue explicitly; the
/// compact `default` profile has its own dedicated coverage in
/// `mcp_dispatch.rs::profiles`.
async fn dispatch_request(
    client: &ApiClient,
    req: JsonRpcRequest,
) -> Option<daruma_mcp::JsonRpcResponse> {
    dispatch_request_with_profile(client, ToolProfile::Full, req).await
}

/// Lifecycle + task binding (OSS task 019eb65b) through the MCP surface:
/// `daruma_doc_set_status` and `daruma_doc_link_task` round-trip via the
/// PATCH route into the projection, visible through `daruma_doc_get`.
#[tokio::test]
async fn doc_lifecycle_and_task_link_roundtrip() {
    let (addr, token) = spawn_daruma_inline().await;
    let client = ApiClient::new(format!("http://{addr}"), token);

    let pid = create_project_via_mcp(&client, "Demo").await;
    let anchor_task_id = create_task_via_mcp(&client, &pid, "anchor").await;
    let doc_id =
        create_doc_via_mcp_for_task(&client, &pid, "interview", "Doc", &anchor_task_id).await;

    // Fresh doc: default lifecycle status is `active`, linked to its
    // creation-time anchor (the barrier requires one).
    let doc = call_tool(&client, "daruma_doc_get", json!({ "document_id": doc_id })).await;
    assert_eq!(doc["document"]["status"], "active", "got: {doc}");
    assert_eq!(doc["document"]["task_id"], anchor_task_id.as_str(), "got: {doc}");

    // Status change.
    let resp = call_tool(
        &client,
        "daruma_doc_set_status",
        json!({ "document_id": doc_id, "status": "outdated" }),
    )
    .await;
    assert_eq!(resp["success"], true, "set_status must succeed: {resp}");
    let doc = call_tool(&client, "daruma_doc_get", json!({ "document_id": doc_id })).await;
    assert_eq!(doc["document"]["status"], "outdated", "got: {doc}");

    // Materialize a task (plan-only intake) and link the document to it.
    let task = call_tool(
        &client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": "doc link plan", "project_id": pid },
            "tasks": [ { "title": "target" } ]
        }),
    )
    .await;
    let task_id = task["data"]
        .as_array()
        .expect("event envelopes")
        .iter()
        .find_map(|e| e["payload"]["task"]["id"].as_str())
        .expect("task id in materialize response")
        .to_owned();

    let resp = call_tool(
        &client,
        "daruma_doc_link_task",
        json!({ "document_id": doc_id, "task_id": task_id }),
    )
    .await;
    assert_eq!(resp["success"], true, "link_task must succeed: {resp}");
    let doc = call_tool(&client, "daruma_doc_get", json!({ "document_id": doc_id })).await;
    assert_eq!(doc["document"]["task_id"], task_id.as_str(), "got: {doc}");

    // Explicit null unlinks.
    let resp = call_tool(
        &client,
        "daruma_doc_link_task",
        json!({ "document_id": doc_id, "task_id": null }),
    )
    .await;
    assert_eq!(resp["success"], true, "unlink must succeed: {resp}");
    let doc = call_tool(&client, "daruma_doc_get", json!({ "document_id": doc_id })).await;
    assert!(doc["document"].get("task_id").is_none(), "got: {doc}");
}
