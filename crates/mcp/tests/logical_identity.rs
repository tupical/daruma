use axum::{
    extract::State,
    http::HeaderMap,
    routing::{any, get, post},
    Json, Router,
};
use daruma_mcp::{logical_identity::bind, ApiClient};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

async fn start_fixture(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, server)
}

#[tokio::test]
async fn lost_response_keeps_retry_key_and_explicit_reuse_never_registers() {
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new()
        .route(
            "/v1/cloud/agents",
            post(
                |State(s): State<Arc<Mutex<Vec<Value>>>>, Json(v): Json<Value>| async move {
                    let mut seen = s.lock().unwrap();
                    seen.push(v);
                    if seen.len() == 1 {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error": "lost result"})),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            Json(json!({"id":"00000000-0000-4000-8000-000000000001"})),
                        )
                    }
                },
            ),
        )
        .route(
            "/v1/cloud/agents/{id}",
            get(|| async { axum::http::StatusCode::NOT_FOUND }),
        )
        .with_state(seen.clone());
    let (url, server) = start_fixture(app).await;
    let root = std::env::temp_dir().join(format!("logical-retry-{}", uuid::Uuid::new_v4()));
    let client = ApiClient::new(&url, "credential");
    assert!(bind(client.clone(), &root, "builder", "Builder", None)
        .await
        .is_err());
    let saved = std::fs::read_to_string(root.join("builder.json")).unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert!(saved["binding_id"].is_string());
    assert!(saved["logical_agent_id"].is_null());
    bind(client.clone(), &root, "builder", "Builder", None)
        .await
        .unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], seen[1]);
    }
    let reuse = uuid::Uuid::new_v4();
    assert!(bind(client.clone(), &root, "reuse", "Ignored", Some(reuse))
        .await
        .is_err());
    assert!(bind(client, &root, "reuse", "Ignored", None).await.is_err());
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "invalid reuse never falls back to register"
    );
    std::fs::remove_dir_all(root).unwrap();
    server.abort();
}

#[tokio::test]
async fn corrupt_state_and_unwritable_parent_fail_before_http() {
    let root = std::env::temp_dir().join(format!("logical-corrupt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("builder.json"), "partial-json").unwrap();
    let client = ApiClient::new("http://127.0.0.1:1", "credential");
    let error = bind(client.clone(), &root, "builder", "Builder", None)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("invalid identity state"));
    assert_eq!(
        std::fs::read_to_string(root.join("builder.json")).unwrap(),
        "partial-json"
    );
    let blocked = root.join("file-not-directory");
    std::fs::write(&blocked, "occupied").unwrap();
    let error = bind(client, &blocked, "builder", "Builder", None)
        .await
        .err()
        .unwrap();
    assert!(error
        .to_string()
        .contains("cannot create identity directory"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn binding_persists_before_registration_and_propagates_without_rekeying_runtime() {
    let state = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new()
        .route("/v1/cloud/agents", post(|State(s): State<Arc<Mutex<Vec<Value>>>>, Json(v): Json<Value>| async move {
            s.lock().unwrap().push(v.clone());
            Json(json!({"id": "00000000-0000-4000-8000-000000000001", "name": "Builder"}))
        }))
        .route("/v1/cloud/agents/{id}", get(|| async { Json(json!({"id": "00000000-0000-4000-8000-000000000001"})) }))
        .route("/v1/probe", any(|headers: HeaderMap| async move { Json(json!({"logical": headers.get("x-daruma-logical-agent-id").unwrap().to_str().unwrap()})) }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = std::env::temp_dir().join(format!("logical-test-{}", uuid::Uuid::new_v4()));
    let client = ApiClient::new(&url, "same-existing-credential");
    let runtime = client.agent_id().to_owned();
    let first = bind(client.clone(), &root, "builder", "Builder", None)
        .await
        .unwrap();
    let second = bind(client, &root, "builder", "Builder", None)
        .await
        .unwrap();
    assert_eq!(first.logical_agent_id(), second.logical_agent_id());
    assert_eq!(first.agent_id(), runtime);
    let reply = first.get_json("/v1/probe").await.unwrap();
    let init = daruma_mcp::dispatch_request_with_profile(
        &first.clone().with_open_tasks(vec![]),
        daruma_mcp::ToolProfile::Default,
        serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap(),
    )
    .await
    .unwrap();
    let init = serde_json::to_value(init).unwrap();
    assert_eq!(
        init["result"]["_meta"]["daruma/logical_agent_id"],
        "00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(reply["logical"], "00000000-0000-4000-8000-000000000001");
    for reply in [
        first.post_json("/v1/probe", json!({})).await.unwrap(),
        first.put_json("/v1/probe", json!({})).await.unwrap(),
        first.patch_json("/v1/probe", json!({})).await.unwrap(),
        first.delete_json("/v1/probe").await.unwrap(),
    ] {
        assert_eq!(reply["logical"], "00000000-0000-4000-8000-000000000001");
    }
    assert_eq!(
        state.lock().unwrap().len(),
        1,
        "restart verifies saved ID rather than recreating"
    );
    let files: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
    assert_eq!(
        files.len(),
        1,
        "only separate identity document remains after lock release"
    );
    let raw = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    assert!(!raw.contains("same-existing-credential"));
    std::fs::remove_dir_all(root).unwrap();
    server.abort();
}
