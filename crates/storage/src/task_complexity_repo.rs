//! `task_complexity_hints` projection repository (§3.8.3).
//!
//! Unlike most projections in this crate, complexity hints are NOT
//! event-sourced. They are direct output of one batch LLM call invoked
//! by `taskagent_ai_analyze_complexity { plan_id }`; the handler simply
//! upserts the rows. Re-running analysis overwrites the previous row
//! per `task_id` (latest wins); `batch_id` lets callers correlate every
//! row produced by the same run.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use taskagent_domain::ComplexityHint;
use taskagent_shared::{CoreError, PlanId, Result, TaskId};

/// Read/write access to the `task_complexity_hints` projection table.
pub struct TaskComplexityRepo {
    pub(crate) pool: SqlitePool,
}

impl TaskComplexityRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert (or replace on `task_id` collision) a single hint row.
    pub async fn upsert(&self, hint: &ComplexityHint) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO task_complexity_hints \
             (task_id, score, recommended_subtasks, expansion_hint, reasoning, \
              generated_at, batch_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hint.task_id.to_string())
        .bind(hint.score as i64)
        .bind(hint.recommended_subtasks as i64)
        .bind(&hint.expansion_hint)
        .bind(&hint.reasoning)
        .bind(hint.generated_at.to_rfc3339())
        .bind(&hint.batch_id)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Upsert a whole batch in one transaction. Caller supplies the
    /// already-built `ComplexityHint` rows (matching `generated_at` /
    /// `batch_id` is the caller's responsibility — they all came from
    /// the same LLM call).
    pub async fn upsert_batch(&self, hints: &[ComplexityHint]) -> Result<()> {
        if hints.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        for hint in hints {
            sqlx::query(
                "INSERT OR REPLACE INTO task_complexity_hints \
                 (task_id, score, recommended_subtasks, expansion_hint, reasoning, \
                  generated_at, batch_id) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(hint.task_id.to_string())
            .bind(hint.score as i64)
            .bind(hint.recommended_subtasks as i64)
            .bind(&hint.expansion_hint)
            .bind(&hint.reasoning)
            .bind(hint.generated_at.to_rfc3339())
            .bind(&hint.batch_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Fetch the latest hint for a single task, if any.
    pub async fn get(&self, task_id: TaskId) -> Result<Option<ComplexityHint>> {
        let row = sqlx::query(
            "SELECT task_id, score, recommended_subtasks, expansion_hint, reasoning, \
             generated_at, batch_id \
             FROM task_complexity_hints WHERE task_id = ?",
        )
        .bind(task_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_hint).transpose()
    }

    /// Every hint whose `task_id` is referenced by the given plan's
    /// `plan_tasks`. Ordered by `position` so the result lines up with
    /// the plan's task order.
    pub async fn list_by_plan(&self, plan_id: PlanId) -> Result<Vec<ComplexityHint>> {
        let rows = sqlx::query(
            "SELECT h.task_id, h.score, h.recommended_subtasks, h.expansion_hint, \
             h.reasoning, h.generated_at, h.batch_id \
             FROM task_complexity_hints h \
             JOIN plan_tasks pt ON pt.task_id = h.task_id \
             WHERE pt.plan_id = ? \
             ORDER BY pt.position ASC",
        )
        .bind(plan_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_hint).collect()
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_hint(row: &sqlx::sqlite::SqliteRow) -> Result<ComplexityHint> {
    let task_id_s: String = row
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let score: i64 = row
        .try_get("score")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let recommended: i64 = row
        .try_get("recommended_subtasks")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let expansion_hint: String = row
        .try_get("expansion_hint")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let reasoning: String = row
        .try_get("reasoning")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let generated_at_s: String = row
        .try_get("generated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let batch_id: String = row
        .try_get("batch_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let task_id = task_id_s
        .parse::<TaskId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(ComplexityHint {
        task_id,
        score: score.clamp(0, u8::MAX as i64) as u8,
        recommended_subtasks: recommended.clamp(0, u8::MAX as i64) as u8,
        expansion_hint,
        reasoning,
        generated_at: parse_ts(&generated_at_s)?,
        batch_id,
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
    use taskagent_shared::{time, TaskId};

    async fn make_repo() -> (Db, TaskComplexityRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = TaskComplexityRepo::new(db.pool().clone());
        (db, repo)
    }

    /// Seed a minimal `tasks` row so the FK on `task_complexity_hints.task_id`
    /// is satisfied.
    async fn seed_task(db: &Db, title: &str) -> TaskId {
        let id = TaskId::new();
        let now = time::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, title, description, status, priority, \
             created_at, updated_at) \
             VALUES (?, NULL, ?, '', 'todo', 'p2', ?, ?)",
        )
        .bind(id.to_string())
        .bind(title)
        .bind(&now)
        .bind(&now)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    fn sample_hint(task_id: TaskId, batch_id: &str, score: u8) -> ComplexityHint {
        ComplexityHint {
            task_id,
            score,
            recommended_subtasks: 3,
            expansion_hint: "decompose into setup/work/verify".into(),
            reasoning: "moderate scope".into(),
            generated_at: time::now(),
            batch_id: batch_id.into(),
        }
    }

    #[tokio::test]
    async fn upsert_and_get() {
        let (db, repo) = make_repo().await;
        let task_id = seed_task(&db, "do thing").await;
        let hint = sample_hint(task_id, "batch-1", 5);

        repo.upsert(&hint).await.unwrap();
        let got = repo.get(task_id).await.unwrap().unwrap();
        assert_eq!(got.task_id, task_id);
        assert_eq!(got.score, 5);
        assert_eq!(got.batch_id, "batch-1");
    }

    #[tokio::test]
    async fn upsert_overwrites_previous_row() {
        let (db, repo) = make_repo().await;
        let task_id = seed_task(&db, "do thing").await;

        repo.upsert(&sample_hint(task_id, "batch-1", 3))
            .await
            .unwrap();
        repo.upsert(&sample_hint(task_id, "batch-2", 8))
            .await
            .unwrap();

        let got = repo.get(task_id).await.unwrap().unwrap();
        assert_eq!(got.score, 8);
        assert_eq!(got.batch_id, "batch-2");
    }

    #[tokio::test]
    async fn upsert_batch_inserts_all_rows() {
        let (db, repo) = make_repo().await;
        let a = seed_task(&db, "a").await;
        let b = seed_task(&db, "b").await;
        let c = seed_task(&db, "c").await;

        let batch = vec![
            sample_hint(a, "B", 1),
            sample_hint(b, "B", 5),
            sample_hint(c, "B", 9),
        ];
        repo.upsert_batch(&batch).await.unwrap();

        assert_eq!(repo.get(a).await.unwrap().unwrap().score, 1);
        assert_eq!(repo.get(b).await.unwrap().unwrap().score, 5);
        assert_eq!(repo.get(c).await.unwrap().unwrap().score, 9);
    }

    #[tokio::test]
    async fn get_unknown_task_is_none() {
        let (_db, repo) = make_repo().await;
        assert!(repo.get(TaskId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_batch_is_noop() {
        let (_db, repo) = make_repo().await;
        repo.upsert_batch(&[]).await.unwrap();
    }
}
