//! ADR-0009 source chain over HTTP: intake with `plan.source`, `source_chain`
//! on GET /v1/plans/{id} and `?source_chain=true`, `?source=` / `?channel=`
//! search, and POST /v1/sources/extend.

use axum::http::StatusCode;
use serde_json::{json, Value};

mod common;
use common::{json_get, json_post, TestAppBuilder};

const EMAIL: &str = "mailto:dev@x.ru";

async fn post(app: &common::TestApp, uri: &str, body: Value) -> (StatusCode, Value) {
    json_post(app.router.clone(), &app.admin_token, uri, &body.to_string()).await
}

#[tokio::test]
async fn source_chain_read_search_and_extend() {
    let app = TestAppBuilder::default()
        .plan_only_intake(true)
        .build()
        .await;
    let command = |command: Value| {
        post(
            &app,
            "/v1/commands",
            json!({ "command": command, "actor": { "kind": "user" } }),
        )
    };
    let (_, body) = command(json!({ "type": "create_project", "title": "Chain" })).await;
    let project_id = body["data"][0]["payload"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let materialize = |source: Value| {
        command(json!({
            "type": "materialize_plan",
            "plan": { "title": "p", "project_id": project_id, "owner": { "kind": "user" }, "source": source },
            "tasks": [{ "title": "t" }]
        }))
    };
    let plan_id = |body: &Value| {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["payload"]["type"] == "plan_created")
            .map(|e| e["payload"]["plan"]["id"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| panic!("no plan_created in {body}"))
    };

    let (status, body) = materialize(json!({
        "ref": "https://gitlab.x/g/p/-/issues/1",
        "label": "Issue #1",
        "upstream": [{ "ref": EMAIL }, { "label": "Discussion", "occurred_at": "2026-01-01" }]
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = plan_id(&body);
    let (status, body) = materialize(json!({
        "ref": "https://gitlab.x/g/p/-/issues/2",
        "upstream": [{ "ref": EMAIL, "label": "Email" }]
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let second = plan_id(&body);
    let (status, _) = materialize(json!({ "ref": "self://solo" })).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = json_get(
        app.router.clone(),
        &app.admin_token,
        &format!("/v1/plans/{first}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let chain = body["source_chain"].as_array().unwrap();
    assert_eq!(chain.len(), 3, "{body}");
    assert_eq!(chain[0]["label"], "Issue #1");
    assert_eq!(chain[1], json!({ "ref": EMAIL, "label": "Email" }));
    assert!(chain[2]["ref"].as_str().unwrap().starts_with("note:"));
    assert_eq!(chain[2]["occurred_at"], "2026-01-01");

    let list = |query: String| {
        let router = app.router.clone();
        let token = app.admin_token.clone();
        async move {
            let (status, body) =
                json_get(router, &token, &format!("/v1/plans?status=all&{query}")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let mut ids: Vec<String> = body
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["id"].as_str().unwrap().to_owned())
                .collect();
            ids.sort();
            (ids, body)
        }
    };
    let mut both = vec![first.clone(), second.clone()];
    both.sort();
    let (ids, _) = list(format!("project_id={project_id}&source={EMAIL}")).await;
    assert_eq!(ids, both, "a node anywhere in the chain finds both issues");
    let (ids, _) = list(format!("source={EMAIL}")).await;
    assert_eq!(ids, both, "without project_id the search spans projects");
    let (ids, _) = list(format!("project_id={project_id}&channel=HTTPS")).await;
    assert_eq!(ids, both);
    let (ids, _) = list(format!("project_id={project_id}&channel=self")).await;
    assert_eq!(ids.len(), 1);
    let (_, body) = list(format!(
        "project_id={project_id}&source=https://gitlab.x/g/p/-/issues/2&source_chain=true"
    ))
    .await;
    assert_eq!(
        body[0]["source_chain"].as_array().unwrap().len(),
        3,
        "{body}"
    );

    // Extend from the e-mail: attaches above the top node (the discussion).
    let extend = |body: Value| post(&app, "/v1/sources/extend", body);
    let (status, body) =
        extend(json!({ "ref": EMAIL, "upstream": [{ "label": "Client problem" }] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let chain = body["data"]["source_chain"].as_array().unwrap();
    assert_eq!(chain.len(), 3, "{body}");
    assert_eq!(chain[2]["label"], "Client problem");

    let (status, body) =
        extend(json!({ "plan_id": second, "source": { "ref": "self://x" } })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "plan_source_already_set", "{body}");
}
