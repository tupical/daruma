//! Usage-error contract (audit `usage.repeated_error`, task 01a1405b-b756):
//! the id-name aliases of the plan tools and the `status` defaults of the
//! list tools reach the right HTTP route instead of failing with
//! "`<param>` (string) is required".

use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::Request, routing::any, Router};
use daruma_mcp::tools::{call_tool, tool_definitions};
use daruma_mcp::ApiClient;
use serde_json::{json, Value};

type Captured = Arc<Mutex<Vec<(String, Value)>>>;

async fn serve(captured: Captured) -> (ApiClient, tokio::task::JoinHandle<()>) {
    let router = Router::new().fallback(any(move |req: Request<Body>| {
        let captured = captured.clone();
        async move {
            let path = req.uri().path_and_query().unwrap().as_str().to_string();
            let list = path.starts_with("/v1/tasks?") || path.starts_with("/v1/plans?");
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            captured.lock().unwrap().push((path, body));
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
    request_of(tool, args).await.0
}

/// Path and JSON body (`null` when empty) of the single request `tool` sends.
/// A task mutation may follow it with one lifecycle read-back of the task
/// (`GET /v1/tasks/{id}`, see `lifecycle_hint`); that one is not counted.
async fn request_of(tool: &str, args: Value) -> (String, Value) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    call_tool(&client, tool, args)
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e:#}"));
    handle.abort();
    let requests = captured.lock().unwrap().clone();
    let read_back = requests
        .get(1)
        .is_some_and(|(path, body)| path.starts_with("/v1/tasks/") && body.is_null());
    assert_eq!(
        requests.len(),
        1 + usize::from(read_back),
        "{tool}: {requests:?}"
    );
    requests[0].clone()
}

/// Error of `tool`, which must fail before sending any request.
async fn error_of(tool: &str, args: Value) -> String {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    let err = call_tool(&client, tool, args)
        .await
        .expect_err(tool)
        .to_string();
    handle.abort();
    assert!(captured.lock().unwrap().is_empty(), "{tool} sent a request");
    err
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

/// Plan 01a06cfc (argument resolvers): task tools take `task_id` or `id`
/// interchangeably, and an id of the wrong kind fails before any request.
#[tokio::test]
async fn task_tools_accept_id_and_task_id_interchangeably() {
    for args in [json!({"id": "tsk_1"}), json!({"task_id": "tsk_1"})] {
        assert_eq!(path_of("daruma_get", args.clone()).await, "/v1/tasks/tsk_1");
        assert_eq!(
            path_of("daruma_relations", args).await,
            "/v1/tasks/tsk_1/relations"
        );
    }
    assert_eq!(
        path_of("daruma_comment", json!({"id": "tsk_1", "body": "hi"})).await,
        "/v1/tasks/tsk_1/comments"
    );
}

#[tokio::test]
async fn wrong_kind_id_fails_before_any_request() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, handle) = serve(captured.clone()).await;
    let err = call_tool(
        &client,
        "daruma_set_status",
        json!({"id": "pln_1", "status": "done"}),
    )
    .await
    .unwrap_err()
    .to_string();
    let missing = call_tool(
        &client,
        "daruma_comment",
        json!({"plan_id": "pln_1", "body": "x"}),
    )
    .await
    .unwrap_err()
    .to_string();
    handle.abort();
    assert_eq!(
        err,
        "`id` = \"pln_1\" is a plan id, but this tool needs a task id"
    );
    assert_eq!(
        missing,
        "`task_id` (string) is required (alias: `id`); got `plan_id`, which names a plan, not a task"
    );
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
    // `id` and its alias `plan_id` are both listed, so neither is required.
    assert!(required("daruma_plan_next_task").is_null());
}

/// Plan 01a06cfc, task 04c37251 (dispatch on the resolvers): every id
/// family — tasks, plans, runs, documents, sessions, rules, work units,
/// artifacts, relations — takes its own name or the generic `id`.
#[tokio::test]
async fn every_id_family_accepts_its_name_and_generic_id() {
    let cases = [
        (
            "daruma_run_complete",
            "run_id",
            "run_1",
            "/v1/runs/run_1/complete",
        ),
        ("daruma_doc_get", "doc_id", "doc_1", "/v1/documents/doc_1"),
        (
            "daruma_session_get",
            "session_id",
            "ags_1",
            "/v1/sessions/ags_1",
        ),
        ("daruma_rule_get", "rule_id", "rule_1", "/v1/rules/rule_1"),
        (
            "daruma_work_unit_complete",
            "work_unit_id",
            "wu_1",
            "/v1/work-units/wu_1/complete",
        ),
        (
            "daruma_artifact_deprecate",
            "artifact_id",
            "art_1",
            "/v1/artifacts/art_1/deprecate",
        ),
        (
            "daruma_unlink",
            "relation_id",
            "rel_1",
            "/v1/relations/rel_1",
        ),
        (
            "daruma_plan_set_status",
            "id",
            "pln_1",
            "/v1/plans/pln_1/status",
        ),
    ];
    for (tool, name, id, path) in cases {
        let mut extra = json!({});
        if tool == "daruma_plan_set_status" {
            extra = json!({"status": "active"});
        }
        for key in [name, "id"] {
            let mut args = extra.clone();
            args[key] = json!(id);
            assert_eq!(path_of(tool, args).await, path, "{tool} via `{key}`");
        }
    }
}

#[tokio::test]
async fn task_commands_carry_the_resolved_task_id() {
    let (path, body) = request_of("daruma_complete", json!({"task_id": "tsk_1"})).await;
    assert_eq!(path, "/v1/commands");
    assert_eq!(body["command"]["id"], "tsk_1", "{body}");

    let (_, body) = request_of(
        "daruma_split",
        json!({"task_id": "tsk_1", "subtasks": [{"title": "a"}]}),
    )
    .await;
    assert_eq!(body["command"]["parent"], "tsk_1", "{body}");

    let (_, body) = request_of(
        "daruma_bulk_set_status",
        json!({"task_ids": ["tsk_1", "tsk_2"], "status": "done"}),
    )
    .await;
    assert_eq!(body["command"]["ids"], json!(["tsk_1", "tsk_2"]), "{body}");
}

#[tokio::test]
async fn two_id_tools_fill_the_missing_side_from_generic_id() {
    let (path, body) = request_of(
        "daruma_run_start_step",
        json!({"run_id": "run_1", "id": "tsk_1"}),
    )
    .await;
    assert_eq!(path, "/v1/runs/run_1/step/start");
    assert_eq!(body["task_id"], "tsk_1");

    let (path, body) = request_of(
        "daruma_plan_add_task",
        json!({"id": "pln_1", "task_id": "tsk_1"}),
    )
    .await;
    assert_eq!(path, "/v1/plans/pln_1/tasks");
    assert_eq!(body["task_id"], "tsk_1", "{body}");
}

#[tokio::test]
async fn wrong_kind_ids_fail_in_every_family() {
    assert_eq!(
        error_of("daruma_run_complete", json!({"run_id": "tsk_1"})).await,
        "`run_id` = \"tsk_1\" is a task id, but this tool needs a run id"
    );
    assert_eq!(
        error_of("daruma_doc_get", json!({"document_id": "pln_1"})).await,
        "`document_id` = \"pln_1\" is a plan id, but this tool needs a document id"
    );
    assert_eq!(
        error_of(
            "daruma_run_finish_step",
            json!({"run_id": "run_1", "outcome": {"kind": "done"}})
        )
        .await,
        "`task_id` (string) required; `id` is accepted for one of them \
         (a `run_`/`tsk_` prefix picks which)"
    );
    assert_eq!(
        error_of(
            "daruma_handoff_request",
            json!({"from_work_unit_id": "tsk_1", "to_work_unit_id": "wu_2"})
        )
        .await,
        "`from_work_unit_id` = \"tsk_1\" is a task id, but this tool needs a work unit id"
    );
    assert_eq!(
        error_of(
            "daruma_doc_link_task",
            json!({"document_id": "doc_1", "task_id": "pln_1"})
        )
        .await,
        "`task_id` = \"pln_1\" is a plan id, but this tool needs a task id"
    );
    assert_eq!(
        error_of(
            "daruma_doc_link_task",
            json!({"document_id": "doc_1", "id": "pln_1"})
        )
        .await,
        "`id` = \"pln_1\" is a plan id, but this tool needs a task id"
    );
}

/// Task 01a14534 (P2): beside `document_id`, the generic `id` names the task
/// of `daruma_doc_link_task` instead of being dropped as `task_id: null`,
/// which silently unlinked a frozen/outdated/archived document.
#[tokio::test]
async fn doc_link_task_generic_id_fills_the_task_side() {
    let uuid = "019e0000-0000-7000-8000-0000000000aa";
    for (args, task_id) in [
        (
            json!({"document_id": "doc_1", "id": "tsk_1"}),
            json!("tsk_1"),
        ),
        (json!({"document_id": "doc_1", "id": uuid}), json!(uuid)),
        (json!({"doc_id": "doc_1", "id": "tsk_1"}), json!("tsk_1")),
        // `task_id` wins over `id`, and its explicit `null` still unlinks.
        (
            json!({"document_id": "doc_1", "task_id": "tsk_2", "id": "tsk_1"}),
            json!("tsk_2"),
        ),
        (
            json!({"document_id": "doc_1", "task_id": null, "id": "tsk_1"}),
            Value::Null,
        ),
        // An `id` that is the document itself leaves the task side empty.
        (json!({"id": "doc_1"}), Value::Null),
        (json!({"document_id": "doc_1", "id": "doc_1"}), Value::Null),
        (json!({"document_id": "doc_1"}), Value::Null),
    ] {
        let (path, body) = request_of("daruma_doc_link_task", args.clone()).await;
        assert_eq!(path, "/v1/documents/doc_1", "{args}");
        assert_eq!(body, json!({"task_id": task_id}), "{args}");
    }
}

/// Plan 01a06cfc, task 04eacc9f (schemas): the key task/plan tools advertise
/// the alias their resolver accepts, so neither name is `required`, and a
/// call that sends only the alias reaches the server with that id.
#[tokio::test]
async fn key_tool_schemas_advertise_the_accepted_id_alias() {
    let agent = "019e0000-0000-7000-8000-000000000001";
    let cases = [
        ("daruma_get", "id", json!({"task_id": "tsk_1"})),
        (
            "daruma_update",
            "id",
            json!({"task_id": "tsk_1", "title": "t"}),
        ),
        (
            "daruma_set_status",
            "id",
            json!({"task_id": "tsk_1", "status": "todo"}),
        ),
        ("daruma_complete", "id", json!({"task_id": "tsk_1"})),
        (
            "daruma_comment",
            "task_id",
            json!({"id": "tsk_1", "body": "b"}),
        ),
        ("daruma_relations", "task_id", json!({"id": "tsk_1"})),
        (
            "daruma_claim",
            "task_id",
            json!({"id": "tsk_1", "agent_id": agent}),
        ),
        (
            "daruma_release",
            "task_id",
            json!({"id": "tsk_1", "agent_id": agent}),
        ),
        ("daruma_plan_get", "id", json!({"plan_id": "pln_1"})),
        ("daruma_plan_progress", "plan_id", json!({"id": "pln_1"})),
        ("daruma_plan_drain_next", "plan_id", json!({"id": "pln_1"})),
        ("daruma_plan_next_task", "id", json!({"plan_id": "pln_1"})),
    ];
    let defs = tool_definitions();
    for (tool, key, args) in cases {
        let alias = args
            .as_object()
            .unwrap()
            .keys()
            .find(|k| ["id", "task_id", "plan_id"].contains(&k.as_str()))
            .unwrap()
            .clone();
        let schema = &defs.iter().find(|t| t.name == tool).unwrap().input_schema;
        for name in [key, alias.as_str()] {
            assert_eq!(
                schema["properties"][name]["type"], "string",
                "{tool}.{name}"
            );
            let required = schema["required"].as_array().cloned().unwrap_or_default();
            assert!(!required.contains(&json!(name)), "{tool} requires {name}");
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let (client, handle) = serve(captured.clone()).await;
        let result = call_tool(&client, tool, args).await;
        handle.abort();
        let id = if alias == "plan_id" || tool.starts_with("daruma_plan_") {
            "pln_1"
        } else {
            "tsk_1"
        };
        let sent = serde_json::to_string(&*captured.lock().unwrap()).unwrap();
        assert!(
            sent.contains(id),
            "{tool} {{{alias}}} did not reach the server with {id}: {result:?} {sent}"
        );
    }
}
