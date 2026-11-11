use axum::{
    extract::{Request, State},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

#[tokio::test(flavor = "multi_thread")]
async fn real_stdio_adapter_binds_before_initialize_reads() {
    let trace = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
    let app = Router::new().fallback(|State(trace): State<Arc<Mutex<Vec<(String, Option<String>)>>>>, req: Request| async move {
        let path = req.uri().path().to_owned();
        let logical = req.headers().get("x-daruma-logical-agent-id").map(|v| v.to_str().unwrap().to_owned());
        trace.lock().unwrap().push((path.clone(), logical));
        Json(match path.as_str() {
            "/v1/cloud/agents" => json!({"id": "00000000-0000-4000-8000-000000000001"}),
            "/v1/mcp" => json!({"result": {"content": [{"text": "{\"mcp_agent_id\":\"00000000-0000-4000-8000-000000000099\"}"}]}}),
            "/v1/claims" => json!({"claims": []}),
            p if p.starts_with("/v1/cloud/agents/") => json!({"id": "00000000-0000-4000-8000-000000000001"}),
            _ => json!({}),
        })
    }).with_state(trace.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let root = std::env::temp_dir().join(format!("cli-binding-{}", uuid::Uuid::new_v4()));
    let local = root.clone();
    let results = tokio::task::spawn_blocking(move || {
        std::thread::scope(|scope| {
            let launch = || {
                let mut child = Command::new(env!("CARGO_BIN_EXE_daruma"))
                    .args(["--api-url", &base, "--token", "test-credential", "mcp"])
                    .env("DARUMA_AGENT_DIR", &local)
                    .env("DARUMA_LOGICAL_BINDING", "builder")
                    .env("DARUMA_LOGICAL_NAME", "Builder")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                writeln!(
                    child.stdin.take().unwrap(),
                    "{}",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
                )
                .unwrap();
                child.wait_with_output().unwrap()
            };
            let a = scope.spawn(launch);
            let b = scope.spawn(launch);
            [a.join().unwrap(), b.join().unwrap()]
        })
    })
    .await
    .unwrap();
    for result in results {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let response: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(
            response["result"]["_meta"]["daruma/logical_agent_id"],
            "00000000-0000-4000-8000-000000000001"
        );
    }
    let calls = trace.lock().unwrap();
    assert_eq!(
        calls[0].0, "/v1/cloud/agents",
        "bootstrap must precede identity/claim/task lookup: {calls:?}"
    );
    assert!(calls.iter().any(|(p, _)| p == "/v1/claims"));
    assert_eq!(
        calls
            .iter()
            .filter(|(p, _)| p == "/v1/cloud/agents")
            .count(),
        1
    );
    for (path, id) in calls
        .iter()
        .filter(|(p, _)| !p.starts_with("/v1/cloud/agents"))
    {
        assert_eq!(
            id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001"),
            "{path}"
        );
    }
    drop(calls);
    std::fs::remove_dir_all(root).unwrap();
    server.abort();
}
