use daruma_api_dto::query::{ProjectRow, QueryResponse, TaskRow};

#[test]
fn project_without_tasks_remains_in_the_result() {
    let response = r#"{"query_contract":"daruma_query_v1","resource":"projects","semantics_version":"task_start_v1","query_hash":"test","items":[{"id":"00000000-0000-0000-0000-000000000001","title":"Empty","tasks":[]}],"next_cursor":null,"has_more":false,"complete":true,"evaluated_at":"2026-01-01T00:00:00Z","consistency":"request_snapshot"}"#;
    let page: QueryResponse<ProjectRow> = serde_json::from_str(response).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].tasks.as_ref().unwrap().len(), 0);
}

#[test]
fn selected_null_is_distinct_from_an_unselected_field() {
    let row: TaskRow = serde_json::from_str(r#"{"project_id":null,"assignee":null}"#).unwrap();
    assert_eq!(row.project_id, Some(None));
    assert_eq!(row.assignee, Some(None));
    assert_eq!(row.revision, None);
    assert_eq!(serde_json::to_value(row).unwrap(), serde_json::json!({"project_id":null,"assignee":null}));
}
