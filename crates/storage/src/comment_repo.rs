//! Comment projection repository — materialises comment-related events into
//! the `comments` SQLite table.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use daruma_domain::{Actor, Comment, CommentKind, CommentPatch};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CommentId, CoreError, ProjectId, Result, TaskId};

/// Read/write access to the `comments` projection table.
pub struct CommentRepo {
    pub(crate) pool: SqlitePool,
}

impl CommentRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// List all non-deleted comments for a task, ordered by creation time.
    pub async fn list_for_task(&self, task_id: TaskId) -> Result<Vec<Comment>> {
        let rows = sqlx::query(
            "SELECT id, task_id, parent_id, author_json, body, kind, \
             created_at, edited_at, deleted_at \
             FROM comments \
             WHERE task_id = ? AND deleted_at IS NULL \
             ORDER BY created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_comment).collect()
    }

    /// Search non-deleted comment bodies with SQLite LIKE.
    pub async fn search_body(
        &self,
        pattern: &str,
        project_id: Option<ProjectId>,
        limit: usize,
    ) -> Result<Vec<Comment>> {
        let rows = match project_id {
            Some(project_id) => {
                sqlx::query(
                    "SELECT c.id, c.task_id, c.parent_id, c.author_json, c.body, c.kind, \
                     c.created_at, c.edited_at, c.deleted_at \
                     FROM comments c \
                     JOIN tasks t ON t.id = c.task_id \
                     WHERE c.deleted_at IS NULL AND t.project_id = ? \
                       AND c.body LIKE ? ESCAPE '\\' COLLATE NOCASE \
                     ORDER BY c.created_at DESC LIMIT ?",
                )
                .bind(project_id.to_string())
                .bind(pattern)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, task_id, parent_id, author_json, body, kind, \
                     created_at, edited_at, deleted_at \
                     FROM comments \
                     WHERE deleted_at IS NULL AND body LIKE ? ESCAPE '\\' COLLATE NOCASE \
                     ORDER BY created_at DESC LIMIT ?",
                )
                .bind(pattern)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_comment).collect()
    }

    /// Get a single comment by id (returns `Some` even if soft-deleted).
    pub async fn get(&self, id: CommentId) -> Result<Option<Comment>> {
        let row = sqlx::query(
            "SELECT id, task_id, parent_id, author_json, body, kind, \
             created_at, edited_at, deleted_at \
             FROM comments WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_comment).transpose()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert or replace a comment row.
    pub async fn upsert_comment(&self, c: &Comment) -> Result<()> {
        let author_json =
            serde_json::to_string(&c.author).map_err(|e| CoreError::serde(e.to_string()))?;
        let parent_id = c.parent_id.map(|p| p.to_string());
        let kind = c.kind.map(|k| k.as_str().to_string());
        let edited_at = c.edited_at.map(|t| t.to_rfc3339());
        let deleted_at = c.deleted_at.map(|t| t.to_rfc3339());

        sqlx::query(
            "INSERT OR REPLACE INTO comments \
             (id, task_id, parent_id, author_json, body, kind, created_at, edited_at, deleted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(c.id.to_string())
        .bind(c.task_id.to_string())
        .bind(parent_id)
        .bind(author_json)
        .bind(&c.body)
        .bind(kind)
        .bind(c.created_at.to_rfc3339())
        .bind(edited_at)
        .bind(deleted_at)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }

    /// Apply a single event envelope, updating the `comments` projection.
    ///
    /// Non-comment events are silently ignored.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        match &envelope.payload {
            Event::CommentAdded { comment } => {
                self.upsert_comment(comment).await?;
            }

            Event::CommentEdited {
                comment_id,
                patch,
                edited_at,
                ..
            } => {
                if let Some(mut comment) = self.get(*comment_id).await? {
                    apply_patch(&mut comment, patch, *edited_at);
                    self.upsert_comment(&comment).await?;
                }
            }

            Event::CommentDeleted {
                comment_id,
                deleted_at,
                ..
            } => {
                sqlx::query("UPDATE comments SET deleted_at = ? WHERE id = ?")
                    .bind(deleted_at.to_rfc3339())
                    .bind(comment_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }

            // Non-comment events are ignored by this repo.
            _ => {}
        }

        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn apply_patch(comment: &mut Comment, patch: &CommentPatch, edited_at: DateTime<Utc>) {
    if let Some(body) = &patch.body {
        comment.body = body.clone();
    }
    comment.edited_at = Some(edited_at);
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_comment(row: &sqlx::sqlite::SqliteRow) -> Result<Comment> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id_s: String = row
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let parent_id_s: Option<String> = row
        .try_get("parent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let author_json: String = row
        .try_get("author_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let body: String = row
        .try_get("body")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind_s: Option<String> = row
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let edited_at_s: Option<String> = row
        .try_get("edited_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let deleted_at_s: Option<String> = row
        .try_get("deleted_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<CommentId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let task_id = task_id_s
        .parse::<TaskId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let parent_id = parent_id_s
        .map(|s| {
            s.parse::<CommentId>()
                .map_err(|e| CoreError::serde(e.to_string()))
        })
        .transpose()?;
    let author: Actor =
        serde_json::from_str(&author_json).map_err(|e| CoreError::serde(e.to_string()))?;
    let kind = kind_s
        .as_deref()
        .map(CommentKind::from_str)
        .transpose()
        .map_err(CoreError::serde)?;

    Ok(Comment {
        id,
        task_id,
        author,
        body,
        parent_id,
        kind,
        created_at: parse_ts(&created_at_s)?,
        edited_at: edited_at_s.map(|s| parse_ts(&s)).transpose()?,
        deleted_at: deleted_at_s.map(|s| parse_ts(&s)).transpose()?,
    })
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CoreError::serde(e.to_string()))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, Comment, CommentKind, CommentPatch, NewComment};
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::{CommentId, TaskId};

    async fn make_repo() -> CommentRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        CommentRepo::new(db.pool().clone())
    }

    fn make_comment(task_id: TaskId) -> Comment {
        use daruma_shared::time;
        let nc = NewComment {
            id: Some(CommentId::new()),
            task_id,
            body: "hello".to_string(),
            parent_id: None,
            kind: None,
        };
        Comment::from_new(nc, Actor::user(), time::now())
    }

    fn make_comment_with_kind(task_id: TaskId, kind: CommentKind) -> Comment {
        use daruma_shared::time;
        let nc = NewComment {
            id: Some(CommentId::new()),
            task_id,
            body: "research note".to_string(),
            parent_id: None,
            kind: Some(kind),
        };
        Comment::from_new(nc, Actor::user(), time::now())
    }

    #[tokio::test]
    async fn apply_comment_added_and_list() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment(task_id);

        let env = EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        );
        repo.apply_event(&env).await.unwrap();

        let list = repo.list_for_task(task_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, comment.id);
        assert_eq!(list[0].body, "hello");
    }

    #[tokio::test]
    async fn apply_comment_edited_updates_body() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment(task_id);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        ))
        .await
        .unwrap();

        let edited_at = daruma_shared::time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentEdited {
                comment_id: comment.id,
                task_id,
                patch: CommentPatch {
                    body: Some("updated".to_string()),
                },
                edited_at,
            },
        ))
        .await
        .unwrap();

        let got = repo.get(comment.id).await.unwrap().unwrap();
        assert_eq!(got.body, "updated");
        assert!(got.edited_at.is_some());
    }

    #[tokio::test]
    async fn soft_delete_excludes_from_list() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment(task_id);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        ))
        .await
        .unwrap();

        let deleted_at = daruma_shared::time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentDeleted {
                comment_id: comment.id,
                task_id,
                deleted_at,
            },
        ))
        .await
        .unwrap();

        // list excludes soft-deleted
        let list = repo.list_for_task(task_id).await.unwrap();
        assert!(list.is_empty());

        // get still returns the row (with deleted_at set)
        let got = repo.get(comment.id).await.unwrap().unwrap();
        assert!(got.deleted_at.is_some());
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let repo = make_repo().await;
        let result = repo.get(CommentId::new()).await.unwrap();
        assert!(result.is_none());
    }

    // ── §3.8.8: Comment.kind round-tripping ────────────────────────────────────

    #[tokio::test]
    async fn insert_without_kind_round_trips_as_none() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment(task_id); // kind: None
        assert!(comment.kind.is_none());

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(comment.id).await.unwrap().unwrap();
        assert_eq!(got.kind, None);

        let list = repo.list_for_task(task_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, None);
    }

    #[tokio::test]
    async fn insert_with_kind_round_trips_via_select() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment_with_kind(task_id, CommentKind::Research);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(comment.id).await.unwrap().unwrap();
        assert_eq!(got.kind, Some(CommentKind::Research));

        let list = repo.list_for_task(task_id).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, Some(CommentKind::Research));
    }

    #[tokio::test]
    async fn each_comment_kind_round_trips() {
        let repo = make_repo().await;
        let task_id = TaskId::new();

        for kind in CommentKind::ALL {
            let comment = make_comment_with_kind(task_id, kind);
            repo.apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::CommentAdded {
                    comment: comment.clone(),
                },
            ))
            .await
            .unwrap();
            let got = repo.get(comment.id).await.unwrap().unwrap();
            assert_eq!(got.kind, Some(kind), "round-trip failed for {kind:?}");
        }
    }

    #[tokio::test]
    async fn edit_preserves_kind() {
        let repo = make_repo().await;
        let task_id = TaskId::new();
        let comment = make_comment_with_kind(task_id, CommentKind::Intent);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentAdded {
                comment: comment.clone(),
            },
        ))
        .await
        .unwrap();

        let edited_at = daruma_shared::time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::CommentEdited {
                comment_id: comment.id,
                task_id,
                patch: CommentPatch {
                    body: Some("revised intent".to_string()),
                },
                edited_at,
            },
        ))
        .await
        .unwrap();

        let got = repo.get(comment.id).await.unwrap().unwrap();
        assert_eq!(got.body, "revised intent");
        // kind survives the edit — `CommentPatch` deliberately can't mutate it.
        assert_eq!(got.kind, Some(CommentKind::Intent));
    }
}
