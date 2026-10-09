use axum::{routing::get, Json, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

async fn call(args: Value) -> Value {
    let router = Router::new()
        .route(
            "/v1/tasks/tsk_1",
            get(|| async {
                Json(
                    json!({"id":"tsk_1","status":"todo","priority":"p0","title":"T",
                            "description":"d ".repeat(2000)}),
                )
            }),
        )
        .route(
            "/v1/tasks/tsk_1/comments",
            get(|| async { Json(json!([{"id":"c1","body":"b ".repeat(2000)}])) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = ApiClient::new(format!("http://{addr}"), "test-token");
    let out = call_tool(&client, "daruma_get", args).await.unwrap();
    server.abort();
    out
}

#[tokio::test]
async fn without_comments_param_response_is_unchanged() {
    let out = call(json!({"id":"tsk_1"})).await;
    assert!(out.get("comments").is_none());
    assert_eq!(out["id"], "tsk_1");
}

#[tokio::test]
async fn comments_true_attaches_full_comment_array() {
    let out = call(json!({"id":"tsk_1","comments":true,"max_tokens":50})).await;
    let comments = out["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"].as_str().unwrap().len(), 4000);
    assert!(out["description"].as_str().unwrap().len() < 4000);
}
