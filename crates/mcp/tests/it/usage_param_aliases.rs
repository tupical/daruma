//! Usage-error contract (audit `usage.repeated_error`, task 01a1405b-b756):
//! the id-name aliases of the plan tools and the `status` defaults of the
//! list tools reach the right HTTP route instead of failing with
//! "`<param>` (string) is required".

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, routing::any, Router};
use daruma_mcp::tools::{call_tool, tool_definitions};
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

async fn serve(captured: Arc<Mutex<Vec<String>>>) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let captured = captured.clone();
        async move {
            let path = req.uri().path_and_query().unwrap().as_str().to_string();
            let list = path.starts_with("/v1/tasks?") || path.starts_with("/v1/plans?");
            captured.lock().unwrap().push(path);
            axum::Json(if list { json!([]) } else { json!({}) })
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (ApiClient::new(base, "test-token"), handle)
}

async fn path_of(tool: &str, args: Value) -> String {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    call_tool(&client, tool, args)
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e:#}"));
    handle.abort();
    let paths = captured.lock().unwrap().clone();
    assert_eq!(paths.len(), 1, "{tool}: {paths:?}");
    paths[0].clone()
}

#[tokio::test]
async fn plan_next_task_accepts_plan_id_alias_and_optional_run_id() {
    assert_eq!(
        path_of("daruma_plan_next_task", json!({"plan_id": "pln_1"})).await,
        "/v1/plans/pln_1/next-task?claim_ttl_secs=0"
    );
    assert_eq!(
        path_of(
            "daruma_plan_next_task",
            json!({"id": "pln_1", "run_id": "run_1", "claim_ttl_secs": 60})
        )
        .await,
        "/v1/plans/pln_1/next-task?run_id=run_1&claim_ttl_secs=60"
    );
}

#[tokio::test]
async fn plan_progress_accepts_id_alias() {
    assert_eq!(
        path_of("daruma_plan_progress", json!({"id": "pln_1"})).await,
        "/v1/plans/pln_1/progress"
    );
    assert_eq!(
        path_of("daruma_plan_progress", json!({"plan_id": "pln_1"})).await,
        "/v1/plans/pln_1/progress"
    );
}

#[tokio::test]
async fn missing_plan_id_error_names_the_alias() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    let err = call_tool(&client, "daruma_plan_progress", json!({}))
        .await
        .unwrap_err()
        .to_string();
    handle.abort();
    assert_eq!(err, "`plan_id` (string) is required (alias: `id`)");
    assert!(captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_tools_default_status_when_omitted() {
    assert_eq!(
        path_of("daruma_list", json!({"project_id": "all"})).await,
        "/v1/tasks?status=active"
    );
    assert_eq!(
        path_of("daruma_plan_list", json!({"project_id": "all"})).await,
        "/v1/plans?status=draft%2Cactive"
    );
    // An explicit status still wins.
    assert_eq!(
        path_of(
            "daruma_list",
            json!({"project_id": "all", "status": "todo"})
        )
        .await,
        "/v1/tasks?status=todo"
    );
}

#[test]
fn schemas_no_longer_require_defaulted_params() {
    let defs = tool_definitions();
    let required =
        |name: &str| defs.iter().find(|t| t.name == name).unwrap().input_schema["required"].clone();
    assert!(required("daruma_list").is_null());
    assert!(required("daruma_plan_list").is_null());
    assert_eq!(required("daruma_plan_next_task"), json!(["id"]));
}
