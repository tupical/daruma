//! HTTP API tests for the audit primitives (Audit primitives tasks A/B/C):
//! findings upsert/list/acknowledge/resolve-missing, plus the read-tracking and
//! stuck-task heuristics end-to-end through the router.

mod common;

use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
    Router,
};
use common::{json_get, json_post, TestAppBuilder};
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

async fn create_project(app: &Router, token: &str) -> String {
    let (s, ev) = json_post(
        app.clone(),
        token,
        "/v1/commands",
        r#"{"command":{"type":"create_project","title":"Audit Demo"}}"#,
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
async fn finding_upsert_is_idempotent_and_filterable() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project_id = create_project(&app.router, &token).await;

    let body = json!({
        "finding": {
            "project_id": project_id,
            "check_key": "doc.unread",
            "category": "staleness",
            "severity": "warn",
            "title": "Interview doc unread for 40 days",
            "detail": "Last read never",
            "remediation": "Review the interview doc",
            "source": "script"
        }
    })
    .to_string();

    // First record opens a finding.
    let (s, first) = json_post(app.router.clone(), &token, "/v1/audit/findings", &body).await;
    assert_eq!(s, StatusCode::OK, "record: {first}");
    let id1 = first["finding"]["id"].as_str().expect("id").to_string();
    assert_eq!(first["finding"]["status"], json!("open"));

    // Re-record the same check+entity → same id, no duplicate.
    let (s, second) = json_post(app.router.clone(), &token, "/v1/audit/findings", &body).await;
    assert_eq!(s, StatusCode::OK, "re-record: {second}");
    assert_eq!(second["finding"]["id"].as_str().unwrap(), id1);

    // List by project: exactly one finding.
    let (s, listed) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/findings?project_id={project_id}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {listed}");
    assert_eq!(listed["findings"].as_array().unwrap().len(), 1);

    // Severity filter that doesn't match → empty.
    let (_s, none) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/findings?project_id={project_id}&severity=error"),
    )
    .await;
    assert_eq!(none["findings"].as_array().unwrap().len(), 0);

    // Acknowledge it.
    let (s, ack) = json_post(
        app.router.clone(),
        &token,
        &format!("/v1/audit/findings/{id1}/status"),
        r#"{"status":"acknowledged"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ack: {ack}");
    let (_s, got) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/findings/{id1}"),
    )
    .await;
    assert_eq!(got["finding"]["status"], json!("acknowledged"));
}

#[tokio::test]
async fn resolve_missing_auto_resolves_unseen_findings() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project_id = create_project(&app.router, &token).await;

    let mk = |task_uuid: &str| {
        json!({
            "finding": {
                "project_id": project_id,
                "task_id": format!("tsk_{task_uuid}"),
                "check_key": "task.stuck",
                "category": "staleness",
                "severity": "warn",
                "title": "stuck",
                "source": "script"
            }
        })
        .to_string()
    };

    let (_s, a) = json_post(
        app.router.clone(),
        &token,
        "/v1/audit/findings",
        &mk("11111111-1111-7111-8111-111111111111"),
    )
    .await;
    let (_s, b) = json_post(
        app.router.clone(),
        &token,
        "/v1/audit/findings",
        &mk("22222222-2222-7222-8222-222222222222"),
    )
    .await;
    let id_a = a["finding"]["id"].as_str().unwrap().to_string();
    let id_b = b["finding"]["id"].as_str().unwrap().to_string();

    // Next run only re-saw `a`; `b` must auto-resolve.
    let resolve_body = json!({
        "project_id": project_id,
        "check_key": "task.stuck",
        "seen": [id_a]
    })
    .to_string();
    let (s, resolved) = json_post(
        app.router.clone(),
        &token,
        "/v1/audit/findings/resolve-missing",
        &resolve_body,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve-missing: {resolved}");
    assert_eq!(resolved["resolved"], json!(1));

    let (_s, got_b) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/findings/{id_b}"),
    )
    .await;
    assert_eq!(got_b["finding"]["status"], json!("resolved"));
}

#[tokio::test]
async fn doc_read_tracking_and_unread_heuristic() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project_id = create_project(&app.router, &token).await;

    // Create a document.
    let create_body = json!({
        "new_doc": { "project_id": project_id, "kind": "interview", "title": "Interview" }
    })
    .to_string();
    let (s, created) = json_post(app.router.clone(), &token, "/v1/documents", &create_body).await;
    assert_eq!(s, StatusCode::CREATED, "create doc: {created}");
    let doc_id = created["data"]["document_id"].as_str().unwrap().to_string();

    let contains_doc = |v: &Value, id: &str| -> bool {
        v["unread"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["document_id"].as_str() == Some(id))
    };

    // Before any read: the new doc is among the unread (never read). (The
    // project is created bare — the core no longer auto-seeds any documents.)
    let (s, unread0) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/heuristics/unread-documents?project_id={project_id}&days=0"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "unread: {unread0}");
    assert!(contains_doc(&unread0, &doc_id), "new doc unread: {unread0}");

    // Read it → read-tracking stamps last_read_at / read_count. The response is
    // the pre-read snapshot (the read is recorded for the *next* visit), so the
    // stamp is observed on a follow-up GET.
    let (s, got) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/documents/{doc_id}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "get doc: {got}");
    let (_s, got2) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/documents/{doc_id}"),
    )
    .await;
    assert!(
        got2["document"]["last_read_at"].is_string(),
        "read-tracking should stamp last_read_at: {got2}"
    );
    assert!(
        got2["document"]["read_count"].as_u64().unwrap() >= 1,
        "read_count should advance: {got2}"
    );

    // Now "unread in the last 1 day" excludes the freshly-read doc (ours must
    // not be present in the unread set).
    let (_s, unread1) = json_get(
        app.router.clone(),
        &token,
        &format!("/v1/audit/heuristics/unread-documents?project_id={project_id}&days=1"),
    )
    .await;
    assert!(
        !contains_doc(&unread1, &doc_id),
        "freshly read doc should not be unread: {unread1}"
    );
}

#[tokio::test]
async fn stuck_tasks_heuristic_flags_aged_status() {
    let app = TestAppBuilder::default().build().await;
    let token = app.admin_token.clone();
    let project_id = create_project(&app.router, &token).await;

    // Create a task and move it to in_progress.
    let create_task = json!({
        "command": { "type": "create_task", "task": { "title": "work", "project_id": project_id } }
    })
    .to_string();
    let (s, ev) = json_post(app.router.clone(), &token, "/v1/commands", &create_task).await;
    assert_eq!(s, StatusCode::OK, "create_task: {ev}");
    let task_id = ev["data"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| {
            let p = e.get("payload")?;
            if p.get("type")?.as_str()? == "task_created" {
                p.get("task")?.get("id")?.as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("task id");

    let to_ip = json!({
        "command": { "type": "set_status", "id": task_id, "status": "in_progress" }
    })
    .to_string();
    let (s, _e) = json_post(app.router.clone(), &token, "/v1/commands", &to_ip).await;
    assert_eq!(s, StatusCode::OK);

    // threshold_hours=0 → anything already in the status is "stuck".
    let (s, stuck) = json_method(
        app.router.clone(),
        Method::GET,
        &token,
        &format!("/v1/audit/heuristics/stuck-tasks?project_id={project_id}&status=in_progress&threshold_hours=0"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "stuck: {stuck}");
    assert_eq!(stuck["stuck"].as_array().unwrap().len(), 1, "{stuck}");
    assert_eq!(stuck["stuck"][0]["task_id"], json!(task_id));
}
