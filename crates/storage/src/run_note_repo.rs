//! Run-note projection repository — materialises `RunNoteAppended` events
//! into the `run_notes` SQLite table (§3.8.2).

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use daruma_domain::{Actor, RunNote};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, Result, RunId, RunNoteId};

/// Read/write access to the `run_notes` projection table.
pub struct RunNoteRepo {
    pub(crate) pool: SqlitePool,
}

impl RunNoteRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// List notes for a run in chronological order (oldest first).
    ///
    /// `after` is an opaque cursor — pass back the `id` of the last note from
    /// a previous page to continue. `limit` is clamped to `[1, 500]`.
    pub async fn list_for_run(
        &self,
        run_id: RunId,
        limit: u32,
        after: Option<RunNoteId>,
    ) -> Result<Vec<RunNote>> {
        let limit = limit.clamp(1, 500) as i64;

        let rows = if let Some(after) = after {
            // Resolve the cursor's created_at; use a strict `(created_at, id)`
            // comparison so two notes at the same wall-clock are still ordered
            // deterministically.
            let cursor_at_s: Option<String> =
                sqlx::query("SELECT created_at FROM run_notes WHERE id = ? AND run_id = ?")
                    .bind(after.to_string())
                    .bind(run_id.to_string())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?
                    .map(|r| r.try_get::<String, _>("created_at"))
                    .transpose()
                    .map_err(|e| CoreError::storage(e.to_string()))?;

            let Some(cursor_at) = cursor_at_s else {
                // Unknown cursor → empty page (don't error: caller may be paging
                // an unrelated run or a stale id).
                return Ok(vec![]);
            };

            sqlx::query(
                "SELECT id, run_id, body, author_json, created_at \
                 FROM run_notes \
                 WHERE run_id = ? \
                   AND (created_at > ? OR (created_at = ? AND id > ?)) \
                 ORDER BY created_at ASC, id ASC LIMIT ?",
            )
            .bind(run_id.to_string())
            .bind(&cursor_at)
            .bind(&cursor_at)
            .bind(after.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT id, run_id, body, author_json, created_at \
                 FROM run_notes \
                 WHERE run_id = ? \
                 ORDER BY created_at ASC, id ASC LIMIT ?",
            )
            .bind(run_id.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_note).collect()
    }

    /// Fetch a single note by id.
    pub async fn get(&self, id: RunNoteId) -> Result<Option<RunNote>> {
        let row = sqlx::query(
            "SELECT id, run_id, body, author_json, created_at \
             FROM run_notes WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_note).transpose()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Insert (or replace on id collision) a note row.
    pub async fn upsert(&self, note: &RunNote) -> Result<()> {
        let author_json =
            serde_json::to_string(&note.author).map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT OR REPLACE INTO run_notes \
             (id, run_id, body, author_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(note.id.to_string())
        .bind(note.run_id.to_string())
        .bind(&note.body)
        .bind(author_json)
        .bind(note.created_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }

    /// Apply a single event envelope, updating the `run_notes` projection.
    ///
    /// Only `RunNoteAppended` is consumed; all other variants are ignored.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        if let Event::RunNoteAppended {
            run_id,
            note_id,
            body,
            by_actor,
            at,
        } = &envelope.payload
        {
            self.upsert(&RunNote {
                id: *note_id,
                run_id: *run_id,
                body: body.clone(),
                author: by_actor.clone(),
                created_at: *at,
            })
            .await?;
        }
        Ok(())
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_note(row: &sqlx::sqlite::SqliteRow) -> Result<RunNote> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let run_id_s: String = row
        .try_get("run_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let body: String = row
        .try_get("body")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let author_json: String = row
        .try_get("author_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<RunNoteId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let run_id = run_id_s
        .parse::<RunId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let author: Actor =
        serde_json::from_str(&author_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(RunNote {
        id,
        run_id,
        body,
        author,
        created_at: parse_ts(&created_at_s)?,
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
    use daruma_domain::Actor;
    use daruma_events::EventEnvelope;
    use daruma_shared::{time, RunId, RunNoteId};

    async fn make_repo() -> (Db, RunNoteRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = RunNoteRepo::new(db.pool().clone());
        (db, repo)
    }

    /// Insert a minimal `runs` row so FK from `run_notes.run_id` is satisfied.
    /// `runs.plan_id` / `agent_id` have no FK constraints in 0008, so dummy
    /// strings are accepted.
    async fn seed_run(db: &Db) -> RunId {
        let id = RunId::new();
        sqlx::query(
            "INSERT INTO runs (id, plan_id, agent_id, started_at, status) \
             VALUES (?, 'pln_test_seed', 'agt_test_seed', ?, 'active')",
        )
        .bind(id.to_string())
        .bind(time::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    fn sample_note(run_id: RunId) -> RunNote {
        RunNote {
            id: RunNoteId::new(),
            run_id,
            body: "first observation".to_string(),
            author: Actor::user(),
            created_at: time::now(),
        }
    }

    #[tokio::test]
    async fn upsert_and_get() {
        let (_db, repo) = make_repo().await;
        let run_id = seed_run(&_db).await;
        let note = sample_note(run_id);
        repo.upsert(&note).await.unwrap();

        let got = repo.get(note.id).await.unwrap().unwrap();
        assert_eq!(got, note);
    }

    #[tokio::test]
    async fn apply_event_persists_note() {
        let (_db, repo) = make_repo().await;
        let run_id = seed_run(&_db).await;
        let note_id = RunNoteId::new();
        let at = time::now();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::RunNoteAppended {
                run_id,
                note_id,
                body: "hello".to_string(),
                by_actor: Actor::agent("test-agent"),
                at,
            },
        ))
        .await
        .unwrap();

        let notes = repo.list_for_run(run_id, 50, None).await.unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, note_id);
        assert_eq!(notes[0].body, "hello");
        assert!(notes[0].author.is_agent());
    }

    #[tokio::test]
    async fn list_returns_chronological_order() {
        let (_db, repo) = make_repo().await;
        let run_id = seed_run(&_db).await;

        let base = time::now();
        for i in 0..3 {
            repo.upsert(&RunNote {
                id: RunNoteId::new(),
                run_id,
                body: format!("note {i}"),
                author: Actor::user(),
                created_at: base + chrono::Duration::seconds(i),
            })
            .await
            .unwrap();
        }

        let notes = repo.list_for_run(run_id, 50, None).await.unwrap();
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].body, "note 0");
        assert_eq!(notes[1].body, "note 1");
        assert_eq!(notes[2].body, "note 2");
    }

    #[tokio::test]
    async fn list_paginates_via_after_cursor() {
        let (_db, repo) = make_repo().await;
        let run_id = seed_run(&_db).await;

        let base = time::now();
        let mut ids = Vec::new();
        for i in 0..5 {
            let note = RunNote {
                id: RunNoteId::new(),
                run_id,
                body: format!("note {i}"),
                author: Actor::user(),
                created_at: base + chrono::Duration::seconds(i),
            };
            ids.push(note.id);
            repo.upsert(&note).await.unwrap();
        }

        let page1 = repo.list_for_run(run_id, 2, None).await.unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].body, "note 0");
        assert_eq!(page1[1].body, "note 1");

        let page2 = repo
            .list_for_run(run_id, 2, Some(page1[1].id))
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].body, "note 2");
        assert_eq!(page2[1].body, "note 3");

        let page3 = repo
            .list_for_run(run_id, 2, Some(page2[1].id))
            .await
            .unwrap();
        assert_eq!(page3.len(), 1);
        assert_eq!(page3[0].body, "note 4");
    }

    #[tokio::test]
    async fn list_for_unknown_run_is_empty() {
        let (_db, repo) = make_repo().await;
        let notes = repo.list_for_run(RunId::new(), 50, None).await.unwrap();
        assert!(notes.is_empty());
    }
}
