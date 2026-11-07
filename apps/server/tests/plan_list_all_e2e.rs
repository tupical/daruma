//! Corporate scenario (task 01a1ab0c) over a real HTTP server and the MCP
//! dispatcher: a director of two client projects asks `daruma_plan_list`
//! for every project's plans. The tool promised `project_id=all`, yet without
//! a `source` it sent `GET /v1/plans` with no project and the server answered
//! `project_id is required`. `all` now spans every project the token may
//! read, filtered before pagination; an unresolved project asks for one.
//!
//! One test: the inferred-project step sets `DARUMA_PROJECT_ID`, which is
//! process-wide.

use daruma_auth::{Capabilities, Capability, ProjectFilter};
use daruma_mcp::{dispatch_request_with_profile, ApiClient, JsonRpcRequest, ToolProfile};
use serde_json::{json, Value};

mod common;
use common::{mint_pat, spawn_server, TestAppBuilder};

async fn call(client: &ApiClient, name: &str, arguments: Value) -> (bool, Value) {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: "tools/call".into(),
        params: Some(json!({ "name": name, "arguments": arguments })),
    };
    let resp = dispatch_request_with_profile(client, ToolProfile::Full, req)
        .await
        .unwrap();
    assert!(resp.error.is_none(), "{name}: {:?}", resp.error);
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let body = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
    (result["isError"] == true, body)
}

async fn ok(client: &ApiClient, name: &str, arguments: Value) -> Value {
    let (is_error, body) = call(client, name, arguments).await;
    assert!(!is_error, "{name} failed: {body}");
    body
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items in {page}"))
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn materialize(client: &ApiClient, project_id: &str, title: &str, source: Value) -> String {
    let resp = ok(
        client,
        "daruma_plan_materialize",
        json!({
            "plan": { "title": title, "project_id": project_id, "source": source },
            "tasks": [{ "title": format!("{title}: первая задача") }],
            "verbose": true
        }),
    )
    .await;
    resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["payload"]["type"] == "plan_created")
        .and_then(|e| e["payload"]["plan"]["id"].as_str())
        .unwrap_or_else(|| panic!("plan_created in {resp}"))
        .to_owned()
}

/// Every id across the pages of `daruma_plan_list` with `limit=1`.
async fn all_pages(client: &ApiClient, mut arguments: Value) -> Vec<String> {
    let mut out = Vec::new();
    arguments["limit"] = json!(1);
    loop {
        let page = ok(client, "daruma_plan_list", arguments.clone()).await;
        let page_ids = ids(&page);
        assert!(page_ids.len() <= 1, "limit=1 page: {page}");
        out.extend(page_ids);
        match page["next_cursor"].as_str() {
            Some(cursor) if page["has_more"] == true => arguments["cursor"] = json!(cursor),
            _ => return out,
        }
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[tokio::test]
async fn plan_list_all_spans_readable_projects_and_never_errors_on_missing_project() {
    std::env::remove_var("DARUMA_PROJECT_ID");
    let app = TestAppBuilder::default().build().await;
    let addr = spawn_server(&app).await;
    let admin = ApiClient::new(format!("http://{addr}"), app.admin_token.clone());

    // 1. Two client projects with their plans: two for "Ромашка" (one later
    // abandoned), one for "Лютик" from a GitLab issue.
    let project = |title: &'static str| {
        let admin = &admin;
        async move {
            ok(admin, "daruma_project_create", json!({ "title": title })).await["project_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let romashka = project("Ромашка: клиентский портал").await;
    let lyutik = project("Лютик: мобильное приложение").await;
    let portal = materialize(
        &admin,
        &romashka,
        "Портал: личный кабинет",
        json!({ "label": "Планёрка Ромашки" }),
    )
    .await;
    let reports = materialize(
        &admin,
        &romashka,
        "Портал: отчёты",
        json!({ "label": "Созвон с бухгалтерией" }),
    )
    .await;
    let issue = "https://gitlab.example/lyutik/app/-/issues/7";
    let push = materialize(
        &admin,
        &lyutik,
        "Приложение: push-уведомления",
        json!({ "ref": issue, "label": "Issue #7" }),
    )
    .await;
    ok(
        &admin,
        "daruma_plan_set_status",
        json!({ "plan_id": reports, "status": "abandoned", "force": true }),
    )
    .await;
    println!(
        "TRACE romashka={romashka} lyutik={lyutik} portal={portal} reports={reports} (abandoned) push={push}"
    );

    // 2. The reported call: project_id=all, status=active, no source.
    // Expected: both projects' active plans, not `project_id is required`.
    let every = ok(
        &admin,
        "daruma_plan_list",
        json!({ "project_id": "all", "status": "draft,active" }),
    )
    .await;
    println!("TRACE all draft,active -> {:?}", ids(&every));
    assert_eq!(
        sorted(ids(&every)),
        sorted(vec![portal.clone(), push.clone()])
    );
    let projects: Vec<_> = every["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["project_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(projects.contains(&romashka) && projects.contains(&lyutik));

    // 3. Status still filters across projects.
    let abandoned = ok(
        &admin,
        "daruma_plan_list",
        json!({ "project_id": "all", "status": "abandoned" }),
    )
    .await;
    println!("TRACE all abandoned -> {:?}", ids(&abandoned));
    assert_eq!(ids(&abandoned), [reports.clone()]);

    // 4. Pagination walks every project's plan exactly once.
    let paged = all_pages(&admin, json!({ "project_id": "all", "status": "all" })).await;
    println!("TRACE all status=all limit=1 pages -> {paged:?}");
    assert_eq!(
        sorted(paged),
        sorted(vec![portal.clone(), reports.clone(), push.clone()])
    );

    // 5. An explicit project stays one project.
    let one = ok(
        &admin,
        "daruma_plan_list",
        json!({ "project_id": romashka, "status": "all" }),
    )
    .await;
    assert_eq!(
        sorted(ids(&one)),
        sorted(vec![portal.clone(), reports.clone()])
    );

    // 6. Source-filtered `all` keeps its cross-project source path.
    let by_source = ok(
        &admin,
        "daruma_plan_list",
        json!({ "project_id": "all", "status": "all", "source": issue }),
    )
    .await;
    println!("TRACE all source={issue} -> {:?}", ids(&by_source));
    assert_eq!(ids(&by_source), [push.clone()]);

    // 7. No project resolved and no source: a project choice, not a 400.
    let (is_error, selection) =
        call(&admin, "daruma_plan_list", json!({ "status": "active" })).await;
    println!("TRACE no project -> isError={is_error} {selection}");
    assert!(!is_error, "{selection}");
    assert_eq!(selection["needs_project_selection"], true);
    assert_eq!(selection["requested_status"], "active");
    assert!(selection["next_step"]
        .as_str()
        .unwrap()
        .contains("daruma_plan_list is then retried"));
    let offered: Vec<_> = selection["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(offered.contains(&romashka) && offered.contains(&lyutik));

    // 8. An inferred project lists that project only.
    std::env::set_var("DARUMA_PROJECT_ID", &lyutik);
    let (is_error, inferred) = call(&admin, "daruma_plan_list", json!({ "status": "all" })).await;
    std::env::remove_var("DARUMA_PROJECT_ID");
    assert!(!is_error, "{inferred}");
    println!("TRACE inferred lyutik -> {:?}", ids(&inferred));
    assert_eq!(ids(&inferred), [push.clone()]);

    // 9. A token scoped to Ромашка: `all` means "all I may read", the scope
    // filter runs before pagination, and Лютик's project stays forbidden.
    let scoped_token = mint_pat(
        &app.auth_store(),
        Capabilities::from([Capability::PlanRead, Capability::ProjectRead].into_iter()),
        ProjectFilter::Only {
            projects: vec![romashka.parse().unwrap()],
        },
    )
    .await
    .0;
    let scoped = ApiClient::new(format!("http://{addr}"), scoped_token);
    let scoped_all = all_pages(&scoped, json!({ "project_id": "all", "status": "all" })).await;
    println!("TRACE scoped all pages -> {scoped_all:?}");
    assert_eq!(
        sorted(scoped_all),
        sorted(vec![portal.clone(), reports.clone()])
    );
    let first = ok(
        &scoped,
        "daruma_plan_list",
        json!({ "project_id": "all", "status": "all", "limit": 2 }),
    )
    .await;
    assert_eq!(first["has_more"], false, "{first}");
    // The REST page itself: both own rows, and no foreign row behind the cursor.
    let rest = scoped
        .get_json("/v1/plans?project_id=all&status=all&page=true&limit=2")
        .await
        .unwrap();
    println!("TRACE scoped REST page -> {rest}");
    assert_eq!(rest["items"].as_array().unwrap().len(), 2, "{rest}");
    assert_eq!(rest["has_more"], false, "{rest}");
    let scoped_source = ok(
        &scoped,
        "daruma_plan_list",
        json!({ "project_id": "all", "status": "all", "source": issue }),
    )
    .await;
    assert!(ids(&scoped_source).is_empty(), "{scoped_source}");
    let (is_error, foreign) = call(
        &scoped,
        "daruma_plan_list",
        json!({ "project_id": lyutik, "status": "all" }),
    )
    .await;
    println!("TRACE scoped explicit lyutik -> isError={is_error} {foreign}");
    assert!(is_error, "foreign project must be refused: {foreign}");
}
