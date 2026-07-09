//! Relation repository — persistence for typed task-to-task relations.
//!
//! Stores and queries rows in the `task_relations` table introduced by
//! migration `0009_task_relations.sql`.

use chrono::DateTime;
use daruma_domain::{Actor, Relation, RelationKind};
use daruma_events::Event;
use daruma_shared::{CoreError, RelationId, Result, TaskId, Timestamp};
use sqlx::{Row, SqlitePool};

/// Read/write access to the `task_relations` table.
pub struct RelationRepo {
    pub(crate) pool: SqlitePool,
}

impl RelationRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── mutations ─────────────────────────────────────────────────────────────

    /// Insert a new relation row.
    ///
    /// Returns `CoreError::Conflict("relation_exists …")` when a row with the
    /// same `(from_task, to_task, kind)` already exists (UNIQUE violation).
    pub async fn insert(&self, relation: &Relation) -> Result<()> {
        let actor_json = serde_json::to_string(&relation.created_by)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let kind_str = kind_to_str(relation.kind);

        let result = sqlx::query(
            "INSERT INTO task_relations (id, from_task, to_task, kind, created_at, actor_json) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(relation.id.to_string())
        .bind(relation.from.to_string())
        .bind(relation.to.to_string())
        .bind(kind_str)
        .bind(relation.created_at.to_rfc3339())
        .bind(actor_json)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db_err))
                if db_err.message().contains("UNIQUE constraint failed") =>
            {
                Err(CoreError::conflict(format!(
                    "relation_exists: ({}, {}, {})",
                    relation.from, relation.to, kind_str
                )))
            }
            Err(e) => Err(CoreError::storage(e.to_string())),
        }
    }

    /// Update the `kind` of an existing relation row (§3.7.2 / LIN A.3).
    ///
    /// Returns `true` when a row was updated, `false` if no row matched the id.
    /// Used by the command handler to transition `Blocks` edges to
    /// `WasBlocking` when the blocker reaches `Status::Done`.
    pub async fn update_kind(&self, id: RelationId, kind: RelationKind) -> Result<bool> {
        let n = sqlx::query("UPDATE task_relations SET kind = ? WHERE id = ?")
            .bind(kind_to_str(kind))
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
            .rows_affected();
        Ok(n > 0)
    }

    /// Delete a relation by id.
    ///
    /// Returns `true` if a row was deleted, `false` if no row matched.
    pub async fn delete(&self, id: RelationId) -> Result<bool> {
        let n = sqlx::query("DELETE FROM task_relations WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?
            .rows_affected();
        Ok(n > 0)
    }

    // ── queries ───────────────────────────────────────────────────────────────

    /// Fetch a single relation by id.
    pub async fn get(&self, id: RelationId) -> Result<Option<Relation>> {
        let row = sqlx::query(
            "SELECT id, from_task, to_task, kind, created_at, actor_json \
             FROM task_relations WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_relation).transpose()
    }

    /// List all relations where `task_id` appears on either endpoint.
    pub async fn list_by_task(&self, task_id: TaskId) -> Result<Vec<Relation>> {
        let id_str = task_id.to_string();
        let rows = sqlx::query(
            "SELECT id, from_task, to_task, kind, created_at, actor_json \
             FROM task_relations \
             WHERE from_task = ? OR to_task = ? \
             ORDER BY created_at ASC",
        )
        .bind(&id_str)
        .bind(&id_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_relation).collect()
    }

    /// Bulk variant of [`list_by_task`] — fetch all relations where any of the
    /// given task ids appears on either endpoint, in a single round-trip.
    ///
    /// Returns an empty vector when `task_ids` is empty.  Duplicate ids in
    /// `task_ids` are deduplicated by SQL; the result has no Rust-side dedup
    /// guarantee (a single relation appears once because its primary key is
    /// unique).
    pub async fn list_by_task_ids(&self, task_ids: &[TaskId]) -> Result<Vec<Relation>> {
        if task_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Build a `?,?,?` placeholder list for both endpoints (from_task IN ... OR to_task IN ...).
        let placeholders = std::iter::repeat("?")
            .take(task_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, from_task, to_task, kind, created_at, actor_json \
             FROM task_relations \
             WHERE from_task IN ({ph}) OR to_task IN ({ph}) \
             ORDER BY created_at ASC",
            ph = placeholders,
        );

        let mut q = sqlx::query(&sql);
        for id in task_ids {
            q = q.bind(id.to_string());
        }
        for id in task_ids {
            q = q.bind(id.to_string());
        }

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_relation).collect()
    }

    /// List relations where `task_id` is the **blocked** endpoint
    /// (`to_task = task_id AND kind = 'blocks'`).
    ///
    /// Use this to check whether a task has active blockers before allowing
    /// it to transition to Done.
    pub async fn list_blockers(&self, task_id: TaskId) -> Result<Vec<Relation>> {
        let rows = sqlx::query(
            "SELECT id, from_task, to_task, kind, created_at, actor_json \
             FROM task_relations \
             WHERE to_task = ? AND kind = 'blocks' \
             ORDER BY created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_relation).collect()
    }

    /// List relations where `task_id` is the **blocker** endpoint
    /// (`from_task = task_id AND kind = 'blocks'`).
    ///
    /// Use this to find downstream tasks that may become unblocked when
    /// `task_id` transitions to Done.
    pub async fn list_blocks_targets(&self, task_id: TaskId) -> Result<Vec<Relation>> {
        let rows = sqlx::query(
            "SELECT id, from_task, to_task, kind, created_at, actor_json \
             FROM task_relations \
             WHERE from_task = ? AND kind = 'blocks' \
             ORDER BY created_at ASC",
        )
        .bind(task_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_relation).collect()
    }

    // ── event projection ──────────────────────────────────────────────────────

    /// Apply an event to the relation projection.
    ///
    /// Currently a no-op placeholder.  `TaskLinked` / `TaskUnlinked` wiring
    /// is added in W2 once those event variants exist in `daruma-events`.
    // wired in W2
    pub async fn apply_event(&self, _event: &Event) {}
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn kind_to_str(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Blocks => "blocks",
        RelationKind::RelatesTo => "relates_to",
        RelationKind::Duplicates => "duplicates",
        RelationKind::WasBlocking => "was_blocking",
    }
}

fn str_to_kind(s: &str) -> Result<RelationKind> {
    match s {
        "blocks" => Ok(RelationKind::Blocks),
        "relates_to" => Ok(RelationKind::RelatesTo),
        "duplicates" => Ok(RelationKind::Duplicates),
        "was_blocking" => Ok(RelationKind::WasBlocking),
        other => Err(CoreError::serde(format!("unknown relation kind: {other}"))),
    }
}

fn row_to_relation(row: &sqlx::sqlite::SqliteRow) -> Result<Relation> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let from_s: String = row
        .try_get("from_task")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let to_s: String = row
        .try_get("to_task")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind_s: String = row
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let actor_json: String = row
        .try_get("actor_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<RelationId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let from = from_s
        .parse::<TaskId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let to = to_s
        .parse::<TaskId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let kind = str_to_kind(&kind_s)?;
    let created_at: Timestamp = DateTime::parse_from_rfc3339(&created_at_s)
        .map_err(|e| CoreError::serde(e.to_string()))?
        .with_timezone(&chrono::Utc);
    let created_by: Actor =
        serde_json::from_str(&actor_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Relation {
        id,
        from,
        to,
        kind,
        created_at,
        created_by,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::Actor;
    use daruma_shared::time;

    async fn make_repo() -> RelationRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        RelationRepo::new(db.pool().clone())
    }

    fn make_relation(from: TaskId, to: TaskId, kind: RelationKind) -> Relation {
        Relation {
            id: RelationId::new(),
            from,
            to,
            kind,
            created_at: time::now(),
            created_by: Actor::user(),
        }
    }

    // ── 1. insert + get roundtrip ─────────────────────────────────────────────

    #[tokio::test]
    async fn insert_get_roundtrip() {
        let repo = make_repo().await;
        let from = TaskId::new();
        let to = TaskId::new();
        let rel = make_relation(from, to, RelationKind::Blocks);
        let id = rel.id;

        repo.insert(&rel).await.unwrap();

        let fetched = repo.get(id).await.unwrap().expect("relation must exist");
        assert_eq!(fetched.id, rel.id);
        assert_eq!(fetched.from, from);
        assert_eq!(fetched.to, to);
        assert_eq!(fetched.kind, RelationKind::Blocks);
    }

    // ── 2. duplicate insert → CoreError conflict relation_exists ──────────────

    #[tokio::test]
    async fn duplicate_insert_returns_conflict() {
        let repo = make_repo().await;
        let from = TaskId::new();
        let to = TaskId::new();
        let rel1 = make_relation(from, to, RelationKind::Blocks);
        let rel2 = Relation {
            id: RelationId::new(), // different id, same (from, to, kind)
            ..make_relation(from, to, RelationKind::Blocks)
        };

        repo.insert(&rel1).await.unwrap();
        let err = repo.insert(&rel2).await.unwrap_err();

        match err {
            CoreError::Conflict(msg) => {
                assert!(
                    msg.contains("relation_exists"),
                    "expected relation_exists in conflict message, got: {msg}"
                );
            }
            other => panic!("expected Conflict, got: {other:?}"),
        }
    }

    // ── 3. list_by_task returns relations on both endpoints ───────────────────

    #[tokio::test]
    async fn list_by_task_returns_both_directions() {
        let repo = make_repo().await;
        let a = TaskId::new();
        let b = TaskId::new();
        let c = TaskId::new();

        // a → b (blocks), c → a (relates_to)
        repo.insert(&make_relation(a, b, RelationKind::Blocks))
            .await
            .unwrap();
        repo.insert(&make_relation(c, a, RelationKind::RelatesTo))
            .await
            .unwrap();
        // unrelated edge that must NOT appear
        repo.insert(&make_relation(b, c, RelationKind::Duplicates))
            .await
            .unwrap();

        let results = repo.list_by_task(a).await.unwrap();
        assert_eq!(
            results.len(),
            2,
            "should return exactly the 2 edges touching a"
        );
    }

    // ── 4. delete returns true then false ─────────────────────────────────────

    #[tokio::test]
    async fn delete_returns_true_then_false() {
        let repo = make_repo().await;
        let rel = make_relation(TaskId::new(), TaskId::new(), RelationKind::RelatesTo);
        let id = rel.id;

        repo.insert(&rel).await.unwrap();

        let first = repo.delete(id).await.unwrap();
        assert!(first, "first delete must return true");

        let second = repo.delete(id).await.unwrap();
        assert!(!second, "second delete must return false");
    }

    // ── 5. list_blockers / list_blocks_targets ────────────────────────────────

    #[tokio::test]
    async fn list_blockers_and_targets() {
        let repo = make_repo().await;
        let blocker = TaskId::new();
        let blocked = TaskId::new();

        let rel = make_relation(blocker, blocked, RelationKind::Blocks);
        repo.insert(&rel).await.unwrap();

        // list_blockers(blocked) → the relation where blocker blocks blocked
        let blockers = repo.list_blockers(blocked).await.unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].from, blocker);

        // list_blocks_targets(blocker) → same relation seen from blocker side
        let targets = repo.list_blocks_targets(blocker).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].to, blocked);

        // non-Blocks relations must not appear in either list
        let unrelated = repo.list_blockers(blocker).await.unwrap();
        assert!(unrelated.is_empty());
    }

    // ── 6. migration creates the table ────────────────────────────────────────

    #[tokio::test]
    async fn migration_creates_task_relations_table() {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_relations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "table exists and is empty after migration");
    }
}
