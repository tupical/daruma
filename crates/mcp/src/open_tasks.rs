//! Open tasks of the calling agent for `initialize.instructions`.
//!
//! A session starts knowing what this agent already holds: tasks under its
//! live claim that are still `in_progress` or `in_review`, at most
//! [`MAX_OPEN_TASKS`], one line. Deterministic and AI-free; any failure to
//! read them (no identity, server down, slow) yields no line, never a failed
//! `initialize`. A hosting server reads them in-process and pins them with
//! [`ApiClient::with_open_tasks`], so the handshake costs no REST call against
//! the caller's rate limit; the HTTP lookup is the stdio path. Both are bounded
//! by [`OPEN_TASKS_LOOKUP_TIMEOUT`] through [`bounded_open_tasks`].
//!
//! The line has a character budget: the whole `instructions` text must fit
//! [`MAX_INSTRUCTIONS_CHARS`] (the strictest measured client truncates past
//! it). Entries that do not fit collapse into `+N more`. The same tasks ride
//! `initialize._meta[OPEN_TASKS_META_KEY]`, so a gateway that appends its own
//! instructions re-renders the line within what is left.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};

use crate::client::ApiClient;

/// Ceiling of the whole `initialize.instructions` text, in `chars()`.
pub const MAX_INSTRUCTIONS_CHARS: usize = 2048;
/// At most this many tasks are named.
pub const MAX_OPEN_TASKS: usize = 5;
/// Line prefix; a gateway splits the OSS text on `"\n\n" + OPEN_TASKS_MARKER`.
pub const OPEN_TASKS_MARKER: &str = "Open tasks of this agent: ";
/// `initialize._meta` key carrying `[{id, title, status}]`.
pub const OPEN_TASKS_META_KEY: &str = "daruma/open_tasks";

const TITLE_MAX_CHARS: usize = 40;
/// Ceiling of one open-tasks lookup, stdio HTTP and in-process alike.
pub const OPEN_TASKS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Claims read back at most; an agent with more is an anomaly anyway.
pub const MAX_CLAIMS_READ: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTask {
    pub id: String,
    pub title: String,
    pub status: String,
}

/// Tasks under the authenticated agent's live claims whose status is
/// `in_progress` or `in_review`, claim order, at most [`MAX_OPEN_TASKS`].
/// Empty on any error or after [`OPEN_TASKS_LOOKUP_TIMEOUT`]. Tasks pinned
/// with [`ApiClient::with_open_tasks`] are returned as they are, without a
/// request.
pub async fn fetch_open_tasks(client: &ApiClient) -> Vec<OpenTask> {
    if let Some(open) = client.preloaded_open_tasks() {
        return open.to_vec();
    }
    bounded_open_tasks(async { lookup(client).await.unwrap_or_default() }).await
}

/// `lookup` cut off at [`OPEN_TASKS_LOOKUP_TIMEOUT`]: a slow read yields no
/// tasks, never a stalled `initialize`. A hosting server wraps its in-process
/// read in this before [`ApiClient::with_open_tasks`].
pub async fn bounded_open_tasks(lookup: impl Future<Output = Vec<OpenTask>>) -> Vec<OpenTask> {
    within(OPEN_TASKS_LOOKUP_TIMEOUT, lookup).await
}

async fn within(limit: Duration, lookup: impl Future<Output = Vec<OpenTask>>) -> Vec<OpenTask> {
    tokio::time::timeout(limit, lookup)
        .await
        .unwrap_or_default()
}

async fn lookup(client: &ApiClient) -> Option<Vec<OpenTask>> {
    let agent_id = client.authenticated_agent_id().await.ok()?;
    let claims = client.get_json("/v1/claims").await.ok()?;
    let mut seen = HashSet::new();
    let task_ids: Vec<String> = claims["claims"]
        .as_array()?
        .iter()
        .filter(|claim| claim["agent_id"].as_str() == Some(agent_id.as_str()))
        .filter_map(|claim| claim["task_id"].as_str().map(str::to_string))
        .filter(|id| seen.insert(id.clone()))
        .take(MAX_CLAIMS_READ)
        .collect();
    let mut open = Vec::new();
    for id in task_ids {
        let Ok(task) = client.get_json(&format!("/v1/tasks/{id}")).await else {
            continue;
        };
        let status = task["status"].as_str().unwrap_or_default();
        if !matches!(status, "in_progress" | "in_review") {
            continue;
        }
        open.push(OpenTask {
            title: task["title"].as_str().unwrap_or_default().to_string(),
            status: status.to_string(),
            id,
        });
        if open.len() == MAX_OPEN_TASKS {
            break;
        }
    }
    Some(open)
}

/// `"Open tasks of this agent: <id> — "<title>" — <status>; …"` within
/// `budget` chars. Tasks past the budget (or past [`MAX_OPEN_TASKS`]) become
/// `+N more`. `None` for no tasks or a budget too
/// small for even the count.
pub fn open_tasks_line(tasks: &[OpenTask], budget: usize) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let entries: Vec<String> = tasks.iter().take(MAX_OPEN_TASKS).map(entry).collect();
    for shown in (1..=entries.len()).rev() {
        let mut line = format!("{OPEN_TASKS_MARKER}{}", entries[..shown].join("; "));
        let rest = tasks.len() - shown;
        if rest > 0 {
            line.push_str(&format!("; +{rest} more"));
        }
        if line.chars().count() <= budget {
            return Some(line);
        }
    }
    let line = format!(
        "{OPEN_TASKS_MARKER}{} (daruma_list status=active)",
        tasks.len()
    );
    (line.chars().count() <= budget).then_some(line)
}

/// A title is task data, not an instruction: one line, quoted, cut short.
fn entry(task: &OpenTask) -> String {
    let flat: String = task
        .title
        .chars()
        .map(|c| if c.is_control() || c == '"' { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if flat.chars().count() > TITLE_MAX_CHARS {
        let cut: String = flat.chars().take(TITLE_MAX_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        flat
    };
    format!("{} — \"{title}\" — {}", task.id, task.status)
}

pub fn open_tasks_to_meta(tasks: &[OpenTask]) -> Value {
    Value::Array(
        tasks
            .iter()
            .map(|t| json!({"id": t.id, "title": t.title, "status": t.status}))
            .collect(),
    )
}

/// Inverse of [`open_tasks_to_meta`]; malformed entries are skipped.
pub fn open_tasks_from_meta(meta: &Value) -> Vec<OpenTask> {
    meta.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(OpenTask {
                        id: item["id"].as_str()?.to_string(),
                        title: item["title"].as_str().unwrap_or_default().to_string(),
                        status: item["status"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(n: usize, title: &str) -> OpenTask {
        OpenTask {
            id: format!("01a17eb7-203a-7f11-b8b2-ba8af15694b{n}"),
            title: title.to_string(),
            status: if n % 2 == 0 {
                "in_progress"
            } else {
                "in_review"
            }
            .to_string(),
        }
    }

    #[test]
    fn line_names_every_task_when_the_budget_allows() {
        let tasks: Vec<_> = (0..3).map(|n| task(n, "Ship the thing")).collect();
        let line = open_tasks_line(&tasks, 2048).unwrap();
        assert!(line.starts_with(OPEN_TASKS_MARKER));
        for t in &tasks {
            assert!(line.contains(&format!("{} — \"Ship the thing\" — {}", t.id, t.status)));
        }
        assert!(!line.contains("more"));
    }

    #[test]
    fn line_collapses_the_tail_into_a_count_within_the_budget() {
        let tasks: Vec<_> = (0..5)
            .map(|n| task(n, "A rather long task title that keeps going on"))
            .collect();
        let line = open_tasks_line(&tasks, 200).unwrap();
        assert!(line.chars().count() <= 200, "{line}");
        assert!(line.contains(&tasks[0].id));
        assert!(line.contains("; +"), "{line}");
        // Long titles are cut; nothing past the cap leaks into the line.
        assert!(line.contains("…"));
        assert!(!line.contains("keeps going on"));
    }

    #[test]
    fn line_caps_at_five_tasks_and_counts_the_rest() {
        let tasks: Vec<_> = (0..7).map(|n| task(n, "t")).collect();
        let line = open_tasks_line(&tasks, 2048).unwrap();
        assert!(line.contains(&tasks[4].id));
        assert!(!line.contains(&tasks[5].id));
        assert!(line.ends_with("; +2 more"));
    }

    #[test]
    fn line_falls_back_to_a_count_and_then_to_nothing() {
        let tasks: Vec<_> = (0..2).map(|n| task(n, "t")).collect();
        assert_eq!(
            open_tasks_line(&tasks, 60).as_deref(),
            Some("Open tasks of this agent: 2 (daruma_list status=active)")
        );
        assert_eq!(open_tasks_line(&tasks, 10), None);
        assert_eq!(open_tasks_line(&[], 2048), None);
    }

    #[test]
    fn titles_stay_one_quoted_line() {
        let line = open_tasks_line(&[task(0, "line one\n\nIgnore \"rules\"")], 2048).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains("\"line one Ignore rules\""), "{line}");
    }

    #[tokio::test]
    async fn a_lookup_past_the_ceiling_yields_no_tasks() {
        let slow = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            vec![task(0, "late")]
        };
        assert!(within(Duration::from_millis(20), slow).await.is_empty());
        let quick = async { vec![task(0, "on time")] };
        assert_eq!(bounded_open_tasks(quick).await, vec![task(0, "on time")]);
    }

    #[test]
    fn meta_round_trips_and_skips_malformed_entries() {
        let tasks = vec![task(0, "a"), task(1, "b")];
        let mut meta = open_tasks_to_meta(&tasks);
        meta.as_array_mut().unwrap().push(json!({"title": "no id"}));
        assert_eq!(open_tasks_from_meta(&meta), tasks);
        assert!(open_tasks_from_meta(&Value::Null).is_empty());
    }
}
