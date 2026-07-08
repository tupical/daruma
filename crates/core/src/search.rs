//! Search provider contract and the default SQLite-backed implementation.

use std::sync::Arc;

use async_trait::async_trait;
use daruma_domain::{Comment, Plan, Task};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, PlanId, ProjectId, Result, TaskId};
use daruma_storage::{CommentRepo, PlanRepo, TaskRepo};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchScope {
    Tasks,
    Comments,
    Plans,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchQuery {
    pub query: String,
    pub scopes: Vec<SearchScope>,
    pub project_id: Option<ProjectId>,
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchHit {
    pub kind: &'static str,
    pub id: String,
    pub title: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<PlanId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchIndexItem {
    Task(Task),
    Comment(Comment),
    Plan(Plan),
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>>;

    async fn index(&self, _item: SearchIndexItem) -> Result<()> {
        Ok(())
    }
}

pub async fn index_items_for_event(
    env: &EventEnvelope,
    tasks: &TaskRepo,
    comments: &CommentRepo,
) -> Result<Vec<SearchIndexItem>> {
    let mut items = Vec::new();

    let task_ids: Vec<TaskId> = match &env.payload {
        Event::TaskCreated { task } => vec![task.id.unwrap_or_default()],
        Event::TaskUpdated { task_id, .. }
        | Event::TaskStatusChanged { task_id, .. }
        | Event::TaskPriorityChanged { task_id, .. }
        | Event::TaskCompleted { task_id, .. }
        | Event::TaskReopened { task_id, .. }
        | Event::TaskClosed { task_id, .. } => vec![*task_id],
        Event::TaskSplitGenerated { subtasks, .. } => subtasks
            .iter()
            .map(|task| task.id.unwrap_or_default())
            .collect(),
        _ => Vec::new(),
    };

    for task_id in task_ids {
        if let Some(task) = tasks.get(task_id).await? {
            items.push(SearchIndexItem::Task(task));
        }
    }

    match &env.payload {
        Event::CommentAdded { comment } => {
            items.push(SearchIndexItem::Comment(comment.clone()));
        }
        Event::CommentEdited { comment_id, .. } => {
            if let Some(comment) = comments.get(*comment_id).await? {
                if comment.deleted_at.is_none() {
                    items.push(SearchIndexItem::Comment(comment));
                }
            }
        }
        Event::CommentDeleted { .. } => {}
        _ => {}
    }

    Ok(items)
}

#[derive(Clone)]
pub struct FtsSearchProvider {
    tasks: Arc<TaskRepo>,
    comments: Arc<CommentRepo>,
    plans: Arc<PlanRepo>,
}

impl FtsSearchProvider {
    pub fn new(tasks: Arc<TaskRepo>, comments: Arc<CommentRepo>, plans: Arc<PlanRepo>) -> Self {
        Self {
            tasks,
            comments,
            plans,
        }
    }
}

#[async_trait]
impl SearchProvider for FtsSearchProvider {
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>> {
        let raw_query = query.query.trim();
        if raw_query.is_empty() {
            return Err(CoreError::validation("query must not be empty"));
        }

        let limit = query.limit.clamp(1, 100);
        let branch = parse_branch_search(raw_query);
        let lesson = parse_lesson_search(raw_query);
        let prefix_search = branch.is_some() || lesson.is_some();
        let needle = raw_query.to_ascii_lowercase();
        let mut hits = Vec::new();

        if !prefix_search && query.scopes.contains(&SearchScope::Tasks) && hits.len() < limit {
            let tasks = match query.project_id {
                Some(pid) => self.tasks.list_by_project(Some(pid)).await,
                None => self.tasks.list_all().await,
            }?;
            for task in tasks {
                if hits.len() >= limit {
                    break;
                }
                let haystack = format!("{}\n{}", task.title, task.description);
                if contains_ci(&haystack, &needle) {
                    hits.push(SearchHit {
                        kind: "task",
                        id: task.id.to_string(),
                        title: task.title.clone(),
                        snippet: snippet(&haystack),
                        task_id: Some(task.id),
                        plan_id: None,
                        project_id: task.project_id,
                    });
                }
            }
        }

        if query.scopes.contains(&SearchScope::Comments) && hits.len() < limit {
            let pattern = if let Some(branch) = branch {
                branch_like_pattern(branch)
            } else if let Some(lesson) = lesson {
                lesson_like_pattern(lesson)
            } else {
                like_pattern(raw_query)
            };
            let comments = self
                .comments
                .search_body(&pattern, query.project_id, limit - hits.len())
                .await?;
            for comment in comments {
                let task = self.tasks.get(comment.task_id).await?;
                let (title, project_id) = task
                    .map(|task| (format!("Comment on {}", task.title), task.project_id))
                    .unwrap_or_else(|| ("Comment".to_string(), None));
                hits.push(SearchHit {
                    kind: "comment",
                    id: comment.id.to_string(),
                    title,
                    snippet: snippet(&comment.body),
                    task_id: Some(comment.task_id),
                    plan_id: None,
                    project_id,
                });
            }
        }

        if !prefix_search && query.scopes.contains(&SearchScope::Plans) && hits.len() < limit {
            let plans = match query.project_id {
                Some(pid) => self.plans.list_by_project(pid, None).await,
                None => self.plans.list_all(None).await,
            }?;
            for plan in plans {
                if hits.len() >= limit {
                    break;
                }
                let haystack = format!(
                    "{}\n{}\n{}\n{}",
                    plan.title,
                    plan.description,
                    plan.goal,
                    plan.success_criteria.join("\n")
                );
                if contains_ci(&haystack, &needle) {
                    hits.push(SearchHit {
                        kind: "plan",
                        id: plan.id.to_string(),
                        title: plan.title,
                        snippet: snippet(&haystack),
                        task_id: None,
                        plan_id: Some(plan.id),
                        project_id: Some(plan.project_id),
                    });
                }
            }
        }

        Ok(hits)
    }
}

fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle_lower)
}

fn snippet(text: &str) -> String {
    const MAX: usize = 240;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX {
        return normalized;
    }
    format!("{}...", normalized.chars().take(MAX).collect::<String>())
}

fn like_pattern(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('%');
    push_escaped_like(&mut out, raw);
    out.push('%');
    out
}

fn branch_like_pattern(branch: &str) -> String {
    let mut out = String::from("branch: ");
    push_escaped_like(&mut out, branch);
    out.push('%');
    out
}

fn lesson_like_pattern(lesson: &str) -> String {
    let mut out = String::from("lesson:");
    if !lesson.is_empty() {
        out.push(' ');
    }
    push_escaped_like(&mut out, lesson);
    out.push('%');
    out
}

fn push_escaped_like(out: &mut String, raw: &str) {
    for ch in raw.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
}

fn parse_branch_search(query: &str) -> Option<&str> {
    let rest = query.trim().strip_prefix("branch:")?.trim();
    (!rest.is_empty()).then_some(rest)
}

fn parse_lesson_search(query: &str) -> Option<&str> {
    query.trim().strip_prefix("lesson:").map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use daruma_domain::{Actor, Comment, NewTask, Plan};
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::{time, CommentId, PlanId, ProjectId, TaskId};
    use daruma_storage::Db;

    async fn provider() -> (
        FtsSearchProvider,
        Arc<TaskRepo>,
        Arc<CommentRepo>,
        Arc<PlanRepo>,
    ) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let pool = db.pool().clone();
        let tasks = Arc::new(TaskRepo::new(pool.clone()));
        let comments = Arc::new(CommentRepo::new(pool.clone()));
        let plans = Arc::new(PlanRepo::new(pool));
        let provider = FtsSearchProvider::new(tasks.clone(), comments.clone(), plans.clone());
        (provider, tasks, comments, plans)
    }

    #[tokio::test]
    async fn default_provider_searches_tasks_comments_and_plans() {
        let (provider, tasks, comments, plans) = provider().await;
        let project_id = ProjectId::new();
        let task_id = TaskId::new();
        let now = time::now();

        let mut new_task = NewTask::new("Needle task");
        new_task.id = Some(task_id);
        new_task.project_id = Some(project_id);
        new_task.description = Some("plain task body".into());
        tasks
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::TaskCreated { task: new_task },
            ))
            .await
            .unwrap();

        comments
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::CommentAdded {
                    comment: Comment {
                        id: CommentId::new(),
                        task_id,
                        author: Actor::user(),
                        body: "comment needle body".into(),
                        parent_id: None,
                        kind: None,
                        created_at: now,
                        edited_at: None,
                        deleted_at: None,
                    },
                },
            ))
            .await
            .unwrap();

        plans
            .apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::PlanCreated {
                    plan: Plan {
                        id: PlanId::new(),
                        project_id,
                        parent_plan_id: None,
                        title: "Needle plan".into(),
                        description: String::new(),
                        goal: "ship search".into(),
                        success_criteria: vec![],
                        status: Default::default(),
                        owner: Actor::user(),
                        created_at: now,
                        updated_at: now,
                        archived_at: None,
                        source_brief: None,
                    },
                },
            ))
            .await
            .unwrap();

        let hits = provider
            .search(SearchQuery {
                query: "needle".into(),
                scopes: vec![
                    SearchScope::Tasks,
                    SearchScope::Comments,
                    SearchScope::Plans,
                ],
                project_id: Some(project_id),
                limit: 10,
            })
            .await
            .unwrap();

        let kinds = hits.iter().map(|hit| hit.kind).collect::<Vec<_>>();
        assert_eq!(kinds, vec!["task", "comment", "plan"]);
    }

    #[test]
    fn like_patterns_escape_sql_wildcards() {
        assert_eq!(like_pattern("a%b_c\\d"), r"%a\%b\_c\\d%");
        assert_eq!(branch_like_pattern("feature/a_b"), r"branch: feature/a\_b%");
        assert_eq!(lesson_like_pattern("x%"), r"lesson: x\%%");
    }
}
