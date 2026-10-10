use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, http::StatusCode, routing::any, Router};
use daruma_mcp::tools::call_tool;
use daruma_mcp::ApiClient;
use serde_json::json;

#[derive(Debug, Clone)]
struct Captured {
    path: String,
}

fn recording_router(capture: Arc<Mutex<Vec<Captured>>>) -> Router {
    Router::new().fallback(any(move |req: Request<Body>| {
        let capture = capture.clone();
        async move {
            let path = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str().to_string())
                .unwrap_or_else(|| req.uri().path().to_string());
            capture
                .lock()
                .unwrap()
                .push(Captured { path: path.clone() });
            if path == "/v1/projects" {
                (
                    StatusCode::OK,
                    axum::Json(json!([
                        {
                            "id": "prj_daruma_web",
                            "title": "daruma-web",
                            "slug": "daruma-web",
                            "description": "large field omitted by tool response"
                        },
                        {
                            "id": "prj_daruma",
                            "title": "daruma",
                            "slug": "daruma"
                        }
                    ])),
                )
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": format!("unexpected request: {path}")})),
                )
            }
        }
    }))
}

#[tokio::test]
async fn list_without_resolved_project_returns_project_selection() {
    use tokio::net::TcpListener;

    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let router = recording_router(captured.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    let server_handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let client = ApiClient::new(base, "test-token");
    let result = call_tool(
        &client,
        "daruma_list",
        json!({
            "status": "active"
        }),
    )
    .await
    .expect("daruma_list should return project selection");

    server_handle.abort();

    assert_eq!(
        captured
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        // The scope lookup (repo_scopes, migration 0046) precedes the
        // project-selection fallback.
        vec!["/v1/repo-scopes", "/v1/projects"]
    );
    assert_eq!(result["needs_project_selection"], true);
    assert_eq!(result["requested_status"], "active");
    assert_eq!(result["projects"].as_array().unwrap().len(), 2);
    assert_eq!(result["projects"][0]["id"], "prj_daruma_web");
    assert_eq!(result["projects"][0]["title"], "daruma-web");
    assert!(result["projects"][0].get("description").is_none());
    assert_eq!(result["next_tool"]["name"], "daruma_project_use");
}

/// A repo registered twice (`Corporate game` + `corporate-game` / slug `-1`)
/// splits its tasks across two ids. Both listings must point each copy at the
/// other so an agent bound to one id does not silently miss the rest.
async fn serve_duplicated_projects() -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(|req: Request<Body>| async move {
        match req.uri().path() {
            "/v1/projects" => (
                StatusCode::OK,
                axum::Json(json!([
                    {"id": "prj_old", "title": "Corporate game", "slug": "corporate-game"},
                    {"id": "prj_new", "title": "corporate-game", "slug": "corporate-game-1"},
                    {"id": "prj_other", "title": "daruma", "slug": "daruma"}
                ])),
            ),
            path => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"error": format!("unexpected request: {path}")})),
            ),
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, handle)
}

#[tokio::test]
async fn project_list_marks_same_title_projects_as_possible_duplicates() {
    let (base, server) = serve_duplicated_projects().await;
    let client = ApiClient::new(base, "test-token");
    let result = call_tool(&client, "daruma_project_list", json!({}))
        .await
        .expect("daruma_project_list");
    server.abort();

    let projects = result.as_array().expect("project list stays an array");
    assert_eq!(projects.len(), 3);
    assert_eq!(projects[0]["possible_duplicates"][0]["id"], "prj_new");
    assert_eq!(
        projects[0]["possible_duplicates"][0]["slug"],
        "corporate-game-1"
    );
    assert_eq!(projects[1]["possible_duplicates"][0]["id"], "prj_old");
    assert_eq!(
        projects[1]["possible_duplicates"].as_array().unwrap().len(),
        1
    );
    assert!(projects[2].get("possible_duplicates").is_none());
}

#[tokio::test]
async fn project_selection_marks_possible_duplicates() {
    let (base, server) = serve_duplicated_projects().await;
    let client = ApiClient::new(base, "test-token");
    let result = call_tool(&client, "daruma_list", json!({"status": "active"}))
        .await
        .expect("daruma_list should return project selection");
    server.abort();

    assert_eq!(result["needs_project_selection"], true);
    let projects = result["projects"].as_array().unwrap();
    assert_eq!(projects[0]["possible_duplicates"][0]["id"], "prj_new");
    assert_eq!(projects[1]["possible_duplicates"][0]["id"], "prj_old");
    assert!(projects[2].get("possible_duplicates").is_none());
}
