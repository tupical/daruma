//! Output formatting helpers for the `daruma` CLI.
//!
//! Everything in here is pure-function over `serde_json::Value` so the
//! handlers in `main.rs` stay one-liners and the formatting can be unit-
//! tested without spinning up a server.
//!
//! We intentionally accept loosely-typed `Value`s instead of importing
//! `daruma-domain` types: the CLI is a thin wrapper, and tracking
//! domain schema changes in two crates is the wrong trade-off here.

use comfy_table::{ContentArrangement, Table};
use serde_json::Value;

/// Pick the "next" claim-ready task from the project's task list.
///
/// Ordering rule (mirrors what an agent typing `daruma next` would
/// expect): `todo` first, then `in_progress`, then `inbox`. Tasks already
/// `done`/`cancelled` are skipped. Ties broken by priority (`p0` highest)
/// and finally `updated_at` descending.
///
/// Returns `None` when no task qualifies — the caller renders an empty
/// hint in that case.
pub fn pick_next(tasks: &[Value]) -> Option<Value> {
    let status_rank = |s: &str| -> Option<u8> {
        match s {
            "todo" => Some(0),
            "in_progress" => Some(1),
            "inbox" => Some(2),
            _ => None,
        }
    };
    let priority_rank = |p: &str| -> u8 {
        match p {
            "p0" => 0,
            "p1" => 1,
            "p2" => 2,
            "p3" => 3,
            _ => 4,
        }
    };

    let mut candidates: Vec<&Value> = tasks
        .iter()
        .filter(|t| {
            t.get("status")
                .and_then(|v| v.as_str())
                .and_then(status_rank)
                .is_some()
        })
        .collect();

    candidates.sort_by(|a, b| {
        let sa = status_rank(a.get("status").and_then(|v| v.as_str()).unwrap_or("")).unwrap_or(99);
        let sb = status_rank(b.get("status").and_then(|v| v.as_str()).unwrap_or("")).unwrap_or(99);
        sa.cmp(&sb)
            .then_with(|| {
                let pa = priority_rank(a.get("priority").and_then(|v| v.as_str()).unwrap_or(""));
                let pb = priority_rank(b.get("priority").and_then(|v| v.as_str()).unwrap_or(""));
                pa.cmp(&pb)
            })
            .then_with(|| {
                let ua = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                let ub = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                ub.cmp(ua) // newer first
            })
    });

    candidates.into_iter().next().cloned()
}

/// Render a list of tasks as a compact table: id (short) | status | prio | title.
pub fn task_table(tasks: &[Value]) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["id", "status", "prio", "title"]);
    for t in tasks {
        let id = t
            .get("id")
            .and_then(|v| v.as_str())
            .map(short_id)
            .unwrap_or_default();
        let status = t
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let priority = t
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = t
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        table.add_row(vec![id, status, priority, title]);
    }
    table.to_string()
}

/// Render a single task as a vertical key/value table — better for `show`
/// where the user wants the whole record, not a width-clamped row.
pub fn task_detail(task: &Value) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["field", "value"]);
    for key in [
        "id",
        "project_id",
        "title",
        "status",
        "priority",
        "due_at",
        "created_at",
        "updated_at",
        "started_at",
        "completed_at",
    ] {
        let v = task.get(key).map(scalar_string).unwrap_or_default();
        table.add_row(vec![key.to_string(), v]);
    }
    // Description last — it can be multi-line and would otherwise push other
    // rows off-screen if rendered inline.
    if let Some(d) = task.get("description").and_then(|v| v.as_str()) {
        if !d.is_empty() {
            table.add_row(vec!["description".to_string(), d.to_string()]);
        }
    }
    table.to_string()
}

/// Render comments as `at | actor | body`.
pub fn comments_table(comments: &[Value]) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["at", "actor", "body"]);
    for c in comments {
        let at = c
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let actor = c
            .get("author")
            .or_else(|| c.get("actor"))
            .map(scalar_string)
            .unwrap_or_default();
        let body = c
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        table.add_row(vec![at, actor, body]);
    }
    table.to_string()
}

/// Render version history as `v | at | event | fields | summary`.
pub fn history_table(versions: &[Value]) -> String {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["v", "at", "event", "fields", "summary"]);
    for v in versions {
        let number = v
            .get("version_number")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .unwrap_or_default();
        let at = v
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let event = v
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fields = v
            .get("changed_fields")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let summary = v
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        table.add_row(vec![number, at, event, fields, summary]);
    }
    table.to_string()
}

/// Render any JSON scalar/object as a short display string. Objects/arrays
/// fall back to their compact JSON form — sufficient for table cells.
fn scalar_string(v: &Value) -> String {
    match v {
        Value::Null => "".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        _ => v.to_string(),
    }
}

/// Truncate a UUID-shaped id to its first 8 chars so the table stays
/// readable. Callers that need the full id can pass `--json`.
fn short_id(id: &str) -> String {
    if id.len() > 13 {
        // Show the first 8 chars plus an ellipsis — keeps human-eye scan
        // fast while staying distinguishable for >1k tasks.
        format!("{}…", &id[..8])
    } else {
        id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(id: &str, status: &str, priority: &str, updated_at: &str) -> Value {
        json!({
            "id": id,
            "title": format!("task-{id}"),
            "status": status,
            "priority": priority,
            "updated_at": updated_at,
        })
    }

    #[test]
    fn pick_next_prefers_todo_over_in_progress_and_inbox() {
        let tasks = vec![
            t("a", "in_progress", "p2", "2025-01-02"),
            t("b", "todo", "p1", "2025-01-01"),
            t("c", "inbox", "p0", "2025-01-03"),
        ];
        let next = pick_next(&tasks).unwrap();
        assert_eq!(next.get("id").and_then(|v| v.as_str()), Some("b"));
    }

    #[test]
    fn pick_next_breaks_ties_by_priority_then_recency() {
        let tasks = vec![
            t("older-p1", "todo", "p1", "2025-01-01"),
            t("newer-p1", "todo", "p1", "2025-02-01"),
            t("p0", "todo", "p0", "2025-01-01"),
        ];
        let next = pick_next(&tasks).unwrap();
        assert_eq!(next.get("id").and_then(|v| v.as_str()), Some("p0"));
    }

    #[test]
    fn pick_next_skips_done_and_cancelled() {
        let tasks = vec![
            t("d", "done", "p0", "2025-01-01"),
            t("x", "cancelled", "p0", "2025-01-01"),
        ];
        assert!(pick_next(&tasks).is_none());
    }

    #[test]
    fn pick_next_returns_none_on_empty() {
        assert!(pick_next(&[]).is_none());
    }

    #[test]
    fn task_table_renders_header_and_row() {
        // Use a non-UUID title so the assertion below isolates the
        // short-id behavior from the title column.
        let row = json!({
            "id": "019e351b-3f3a-7850-a0bd-85135c0b24d0",
            "title": "plain title",
            "status": "todo",
            "priority": "p1",
        });
        let rendered = task_table(&[row]);
        assert!(rendered.contains("id"));
        assert!(rendered.contains("status"));
        assert!(rendered.contains("todo"));
        assert!(rendered.contains("p1"));
        // Short-id (first 8 chars + ellipsis) should appear in the id column.
        assert!(rendered.contains("019e351b…"));
        // The full UUID tail must not leak into the rendered table.
        assert!(!rendered.contains("85135c0b24d0"));
    }

    #[test]
    fn task_detail_includes_known_fields() {
        let task = json!({
            "id": "abc",
            "title": "demo",
            "status": "todo",
            "priority": "p2",
            "description": "details here",
        });
        let rendered = task_detail(&task);
        assert!(rendered.contains("title"));
        assert!(rendered.contains("demo"));
        assert!(rendered.contains("description"));
        assert!(rendered.contains("details here"));
    }

    #[test]
    fn comments_table_renders_actor_and_body() {
        let comments = vec![json!({
            "created_at": "2025-05-01T00:00:00Z",
            "actor": "agent:mcp",
            "body": "hello world",
        })];
        let rendered = comments_table(&comments);
        assert!(rendered.contains("agent:mcp"));
        assert!(rendered.contains("hello world"));
    }

    #[test]
    fn history_table_includes_version_metadata() {
        let versions = vec![json!({
            "version_number": 2,
            "created_at": "2026-06-02T09:00:00Z",
            "event_type": "task_updated",
            "changed_fields": ["title"],
            "summary": "Task title changed"
        })];
        let rendered = history_table(&versions);
        assert!(rendered.contains("task_updated"));
        assert!(rendered.contains("title"));
        assert!(rendered.contains("Task title changed"));
    }

    #[test]
    fn short_id_truncates_uuid() {
        assert_eq!(
            short_id("019e351b-3f3a-7850-a0bd-85135c0b24d0"),
            "019e351b…"
        );
        assert_eq!(short_id("short"), "short");
    }
}
