//! Document projection repository — materialises `Document*` events
//! into the `documents` SQLite table (PR1 §3-4).

use crate::parse_ts;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use daruma_domain::{Document, DocumentKind, DocumentStatus};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, DocumentId, ProjectId, Result};

use crate::entity_version::{insert_entity_version, update_summary};

/// Append-separator inserted between existing content and an appended chunk.
/// Two newlines so the new block is a fresh markdown paragraph; skipped when
/// the existing document is empty.
const APPEND_SEPARATOR: &str = "\n\n";

/// Read/write access to the `documents` projection table.
pub struct DocumentRepo {
    pub(crate) pool: SqlitePool,
}

impl DocumentRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Fetch a document by id; `None` if not found.
    pub async fn get(&self, id: DocumentId) -> Result<Option<Document>> {
        let row = sqlx::query(
            "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
             last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
             FROM documents WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_document).transpose()
    }

    /// List documents for a project.
    ///
    /// - `kind_filter`: if `Some`, returns only documents of that kind.
    /// - `include_archived`: if `false`, soft-archived rows are hidden.
    ///
    /// Results are ordered by `created_at ASC, id ASC` for deterministic paging.
    pub async fn list_by_project(
        &self,
        project_id: ProjectId,
        kind_filter: Option<DocumentKind>,
        include_archived: bool,
    ) -> Result<Vec<Document>> {
        // Build the query in branches so SQLite gets a stable prepared
        // statement per shape (no dynamic SQL string concatenation).
        let rows = match (kind_filter, include_archived) {
            (Some(kind), true) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
                     last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
                     FROM documents \
                     WHERE project_id = ? AND kind = ? \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .bind(kind.as_str())
            .fetch_all(&self.pool)
            .await,
            (Some(kind), false) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
                     last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
                     FROM documents \
                     WHERE project_id = ? AND kind = ? AND archived_at IS NULL \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .bind(kind.as_str())
            .fetch_all(&self.pool)
            .await,
            (None, true) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
                     last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
                     FROM documents \
                     WHERE project_id = ? \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .fetch_all(&self.pool)
            .await,
            (None, false) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
                     last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
                     FROM documents \
                     WHERE project_id = ? AND archived_at IS NULL \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .fetch_all(&self.pool)
            .await,
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_document).collect()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Apply a single event envelope, updating the `documents` projection.
    ///
    /// Non-document events are silently ignored. Mutations for unknown
    /// document ids (replace/append/rename/archive without a prior create)
    /// are no-ops — this lets us replay event logs out of order without
    /// hard errors.
    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        match &envelope.payload {
            Event::DocumentCreated { document } => {
                let mut tx = self.begin_tx().await?;
                let after = document_value(document)?;
                self.upsert_document_tx(&mut tx, document).await?;
                insert_document_version(&mut tx, envelope, document.id, None, Some(after)).await?;
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentContentReplaced {
                document_id,
                content,
                at,
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.content = content.clone();
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentContentAppended {
                document_id,
                append,
                at,
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.content = if doc.content.is_empty() {
                        append.clone()
                    } else {
                        format!("{}{APPEND_SEPARATOR}{append}", doc.content)
                    };
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentRenamed {
                document_id,
                title,
                at,
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.title = title.clone();
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentArchived { document_id, at } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.archived_at = Some(*at);
                    doc.status = DocumentStatus::Archived;
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentStatusChanged {
                document_id, to, at, ..
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.status = *to;
                    // Keep `archived_at` coherent with the lifecycle status:
                    // entering `archived` stamps it, leaving it clears it.
                    match (*to == DocumentStatus::Archived, doc.archived_at) {
                        (true, None) => doc.archived_at = Some(*at),
                        (false, Some(_)) => doc.archived_at = None,
                        _ => {}
                    }
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            Event::DocumentTaskLinkChanged {
                document_id,
                task_id,
                at,
            } => {
                let mut tx = self.begin_tx().await?;
                if let Some(mut doc) = get_document_tx(&mut tx, *document_id).await? {
                    let before = document_value(&doc)?;
                    doc.task_id = *task_id;
                    doc.updated_at = *at;
                    let after = document_value(&doc)?;
                    self.upsert_document_tx(&mut tx, &doc).await?;
                    insert_document_version(
                        &mut tx,
                        envelope,
                        *document_id,
                        Some(before),
                        Some(after),
                    )
                    .await?;
                }
                tx.commit()
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
            }
            _ => {}
        }
        Ok(())
    }

    // ── private helpers ───────────────────────────────────────────────────────

    async fn begin_tx(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool
            .begin()
            .await
            .map_err(|e| CoreError::storage(e.to_string()))
    }

    async fn upsert_document_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        doc: &Document,
    ) -> Result<()> {
        // Read-tracking columns are carried through so content/title/archive
        // mutations (fetch-then-upsert) preserve them. `DocumentCreated` builds a
        // fresh `Document` with read_count = 0 / NULLs, which is correct.
        sqlx::query(
            "INSERT OR REPLACE INTO documents \
             (id, project_id, kind, title, content, created_at, updated_at, archived_at, \
              last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(doc.id.to_string())
        .bind(doc.project_id.to_string())
        .bind(doc.kind.as_str())
        .bind(&doc.title)
        .bind(&doc.content)
        .bind(doc.created_at.to_rfc3339())
        .bind(doc.updated_at.to_rfc3339())
        .bind(doc.archived_at.as_ref().map(|t| t.to_rfc3339()))
        .bind(doc.last_read_at.as_ref().map(|t| t.to_rfc3339()))
        .bind(doc.last_read_by.as_deref())
        .bind(doc.read_count as i64)
        .bind(doc.status.as_str())
        .bind(doc.task_id.as_ref().map(|t| t.to_string()))
        .bind(doc.trigger_kind.as_deref())
        .bind(doc.consumer.as_deref())
        .execute(&mut **tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }

    // ── read-tracking (migration 0039, Audit primitives task A) ────────────────

    /// Record a read of `id` by `actor`, throttled per (document, actor): a read
    /// within `throttle` of the last read *by the same actor* is a no-op, so
    /// repeated polling doesn't churn the row. Returns `true` when the row was
    /// updated (read counted), `false` when throttled or the document is unknown.
    ///
    /// Not event-sourced: a read is usage telemetry, not a domain fact. The
    /// throttle keeps writes cheap; the indexed `last_read_at` powers the
    /// "documents not read in N days" heuristic.
    pub async fn mark_read(
        &self,
        id: DocumentId,
        actor: &str,
        now: DateTime<Utc>,
        throttle: std::time::Duration,
    ) -> Result<bool> {
        let throttle = chrono::Duration::from_std(throttle)
            .map_err(|e| CoreError::validation(e.to_string()))?;
        // Throttle only against a read by the *same* actor: a different reader
        // always counts, so per-actor recency is preserved.
        let cutoff = (now - throttle).to_rfc3339();
        let res = sqlx::query(
            "UPDATE documents \
             SET last_read_at = ?, last_read_by = ?, read_count = read_count + 1 \
             WHERE id = ? \
               AND NOT (last_read_by = ? AND last_read_at IS NOT NULL AND last_read_at >= ?)",
        )
        .bind(now.to_rfc3339())
        .bind(actor)
        .bind(id.to_string())
        .bind(actor)
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    /// Documents in `project_id` not read since `cutoff` (or never read), oldest
    /// read first. Backward compatible: rows with NULL `last_read_at` (never
    /// read, including pre-0039 documents) are always included. Archived rows are
    /// excluded — a stale archived doc is not an actionable hygiene finding.
    pub async fn list_unread_since(
        &self,
        project_id: ProjectId,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<Document>> {
        let rows = sqlx::query(
            "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
             last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
             FROM documents \
             WHERE project_id = ? AND archived_at IS NULL \
               AND (last_read_at IS NULL OR last_read_at < ?) \
             ORDER BY last_read_at ASC NULLS FIRST, created_at ASC, id ASC",
        )
        .bind(project_id.to_string())
        .bind(cutoff.to_rfc3339())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_document).collect()
    }
}

async fn get_document_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: DocumentId,
) -> Result<Option<Document>> {
    let row = sqlx::query(
        "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at, \
         last_read_at, last_read_by, read_count, status, task_id, trigger_kind, consumer \
         FROM documents WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| CoreError::storage(e.to_string()))?;

    row.as_ref().map(row_to_document).transpose()
}

async fn insert_document_version(
    tx: &mut Transaction<'_, Sqlite>,
    envelope: &EventEnvelope,
    document_id: DocumentId,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    let summary = update_summary("Document", before.as_ref(), after.as_ref());
    insert_entity_version(
        tx,
        "document",
        document_id.to_string(),
        before,
        after,
        envelope,
        summary,
    )
    .await
}

fn document_value(document: &Document) -> Result<Value> {
    serde_json::to_value(document).map_err(|e| CoreError::serde(e.to_string()))
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_document(row: &sqlx::sqlite::SqliteRow) -> Result<Document> {
    let id_s: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let project_id_s: String = row
        .try_get("project_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind_s: String = row
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let title: String = row
        .try_get("title")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let content: String = row
        .try_get("content")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let updated_at_s: String = row
        .try_get("updated_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let archived_at_s: Option<String> = row
        .try_get("archived_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let last_read_at_s: Option<String> = row
        .try_get("last_read_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let last_read_by: Option<String> = row
        .try_get("last_read_by")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let read_count: i64 = row
        .try_get("read_count")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let status_s: String = row
        .try_get("status")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let task_id_s: Option<String> = row
        .try_get("task_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let trigger_kind: Option<String> = row
        .try_get("trigger_kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let consumer: Option<String> = row
        .try_get("consumer")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    let id = id_s
        .parse::<DocumentId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let project_id = project_id_s
        .parse::<ProjectId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let kind = parse_kind(&kind_s)?;
    let status = parse_status(&status_s)?;
    let task_id = task_id_s
        .as_deref()
        .map(|t| t.parse::<daruma_shared::TaskId>())
        .transpose()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let archived_at = archived_at_s.as_deref().map(parse_ts).transpose()?;
    let last_read_at = last_read_at_s.as_deref().map(parse_ts).transpose()?;

    Ok(Document {
        id,
        project_id,
        kind,
        title,
        content,
        status,
        task_id,
        trigger_kind,
        consumer,
        created_at: parse_ts(&created_at_s)?,
        updated_at: parse_ts(&updated_at_s)?,
        archived_at,
        last_read_at,
        last_read_by,
        read_count: read_count.max(0) as u64,
    })
}

fn parse_status(s: &str) -> Result<DocumentStatus> {
    match s {
        "draft" => Ok(DocumentStatus::Draft),
        "active" => Ok(DocumentStatus::Active),
        "outdated" => Ok(DocumentStatus::Outdated),
        "archived" => Ok(DocumentStatus::Archived),
        other => Err(CoreError::serde(format!(
            "unknown document status: {other:?}"
        ))),
    }
}

fn parse_kind(s: &str) -> Result<DocumentKind> {
    match s {
        "interview" => Ok(DocumentKind::Interview),
        "human_log" => Ok(DocumentKind::HumanLog),
        other => Err(CoreError::serde(format!(
            "unknown document kind: {other:?}"
        ))),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, NewDocument};
    use daruma_events::EventEnvelope;
    use daruma_shared::{time, DocumentId, ProjectId};

    async fn make_repo() -> (Db, DocumentRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = DocumentRepo::new(db.pool().clone());
        (db, repo)
    }

    fn seed_doc(project_id: ProjectId, kind: DocumentKind, title: &str) -> Document {
        NewDocument {
            id: None,
            project_id,
            kind,
            title: title.to_string(),
            content: None,
            status: None,
            task_id: None,
            trigger_kind: None,
            consumer: None,
        }
        .into_document(DocumentId::new(), time::now())
    }

    #[tokio::test]
    async fn created_and_retrieved() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::Interview, "Interview");
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated {
                document: doc.clone(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got, doc);
    }

    #[tokio::test]
    async fn document_mutations_write_monotonic_version_records() {
        let (db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::Interview, "Interview");
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentContentReplaced {
                document_id: id,
                content: "updated body".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let versions: Vec<i64> = sqlx::query_scalar(
            "SELECT version_number FROM entity_versions \
             WHERE entity_type = 'document' AND entity_id = ? \
             ORDER BY version_number ASC",
        )
        .bind(id.to_string())
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert_eq!(versions, vec![1, 2]);

        let changed_fields_json: String = sqlx::query_scalar(
            "SELECT changed_fields_json FROM entity_versions \
             WHERE entity_type = 'document' AND entity_id = ? AND version_number = 2",
        )
        .bind(id.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(
            changed_fields_json.contains("content"),
            "content replacement should record changed field, got {changed_fields_json}"
        );
    }

    #[tokio::test]
    async fn content_replaced_overwrites_body() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::HumanLog, "Human Log");
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        let new_at = time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentContentReplaced {
                document_id: id,
                content: "fresh body".to_string(),
                at: new_at,
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got.content, "fresh body");
        assert_eq!(got.updated_at.to_rfc3339(), new_at.to_rfc3339());
    }

    #[tokio::test]
    async fn append_into_empty_skips_separator() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::HumanLog, "Human Log");
        let id = doc.id;
        assert_eq!(doc.content, "", "precondition: doc starts empty");

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentContentAppended {
                document_id: id,
                append: "first entry".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got.content, "first entry");
    }

    #[tokio::test]
    async fn append_into_non_empty_inserts_blank_line() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let mut doc = seed_doc(project_id, DocumentKind::HumanLog, "Human Log");
        doc.content = "# Human Log".to_string();
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentContentAppended {
                document_id: id,
                append: "entry 1".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got.content, "# Human Log\n\nentry 1");
    }

    #[tokio::test]
    async fn renamed_updates_title() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::Interview, "old");
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentRenamed {
                document_id: id,
                title: "new".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert_eq!(got.title, "new");
    }

    #[tokio::test]
    async fn archived_sets_archived_at_and_hides_from_default_list() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::Interview, "Interview");
        let id = doc.id;

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        let archived_at = time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentArchived {
                document_id: id,
                at: archived_at,
            },
        ))
        .await
        .unwrap();

        let got = repo.get(id).await.unwrap().unwrap();
        assert!(got.archived_at.is_some());

        // Default list (include_archived=false) hides the row.
        let default_list = repo.list_by_project(project_id, None, false).await.unwrap();
        assert!(default_list.is_empty());

        // include_archived=true surfaces it again.
        let full_list = repo.list_by_project(project_id, None, true).await.unwrap();
        assert_eq!(full_list.len(), 1);
        assert_eq!(full_list[0].id, id);
    }

    #[tokio::test]
    async fn list_by_project_filters_by_kind() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();

        let interview = seed_doc(project_id, DocumentKind::Interview, "Interview");
        let log = seed_doc(project_id, DocumentKind::HumanLog, "Human Log");

        for d in [&interview, &log] {
            repo.apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::DocumentCreated {
                    document: d.clone(),
                },
            ))
            .await
            .unwrap();
        }

        let all = repo.list_by_project(project_id, None, false).await.unwrap();
        assert_eq!(all.len(), 2);

        let interviews = repo
            .list_by_project(project_id, Some(DocumentKind::Interview), false)
            .await
            .unwrap();
        assert_eq!(interviews.len(), 1);
        assert_eq!(interviews[0].id, interview.id);

        let logs = repo
            .list_by_project(project_id, Some(DocumentKind::HumanLog), false)
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].id, log.id);
    }

    #[tokio::test]
    async fn list_by_project_scopes_to_project() {
        let (_db, repo) = make_repo().await;
        let p1 = ProjectId::new();
        let p2 = ProjectId::new();
        for d in [
            seed_doc(p1, DocumentKind::Interview, "p1 interview"),
            seed_doc(p2, DocumentKind::Interview, "p2 interview"),
        ] {
            repo.apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::DocumentCreated { document: d },
            ))
            .await
            .unwrap();
        }

        let p1_docs = repo.list_by_project(p1, None, false).await.unwrap();
        assert_eq!(p1_docs.len(), 1);
        assert_eq!(p1_docs[0].title, "p1 interview");
    }

    #[tokio::test]
    async fn apply_event_ignores_non_document_events() {
        let (_db, repo) = make_repo().await;
        // A PlanArchived event has nothing to do with documents — should be a no-op.
        let env = EventEnvelope::new(
            Actor::user(),
            Event::PlanArchived {
                plan_id: daruma_shared::PlanId::new(),
                at: time::now(),
            },
        );
        repo.apply_event(&env).await.unwrap();

        let docs = repo
            .list_by_project(ProjectId::new(), None, true)
            .await
            .unwrap();
        assert!(docs.is_empty());
    }

    #[tokio::test]
    async fn mark_read_updates_tracking_and_throttles_same_actor() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let doc = seed_doc(project_id, DocumentKind::Interview, "Interview");
        let id = doc.id;
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentCreated { document: doc },
        ))
        .await
        .unwrap();

        // Precondition: never read.
        let before = repo.get(id).await.unwrap().unwrap();
        assert!(before.last_read_at.is_none());
        assert_eq!(before.read_count, 0);

        let throttle = std::time::Duration::from_secs(3600);
        // First read counts.
        let now = time::now();
        assert!(repo.mark_read(id, "user", now, throttle).await.unwrap());
        let after = repo.get(id).await.unwrap().unwrap();
        assert_eq!(after.read_count, 1);
        assert_eq!(after.last_read_by.as_deref(), Some("user"));
        assert!(after.last_read_at.is_some());

        // Same actor within the throttle window → no-op (no churn).
        assert!(!repo
            .mark_read(id, "user", now + chrono::Duration::minutes(5), throttle)
            .await
            .unwrap());
        assert_eq!(repo.get(id).await.unwrap().unwrap().read_count, 1);

        // A different actor always counts.
        assert!(repo
            .mark_read(id, "agent", now + chrono::Duration::minutes(5), throttle)
            .await
            .unwrap());
        assert_eq!(repo.get(id).await.unwrap().unwrap().read_count, 2);

        // Same actor past the throttle window counts again.
        assert!(repo
            .mark_read(id, "user", now + chrono::Duration::hours(2), throttle)
            .await
            .unwrap());
        assert_eq!(repo.get(id).await.unwrap().unwrap().read_count, 3);
    }

    #[tokio::test]
    async fn list_unread_since_includes_never_read_and_stale() {
        let (_db, repo) = make_repo().await;
        let project_id = ProjectId::new();
        let fresh = seed_doc(project_id, DocumentKind::Interview, "fresh");
        let stale = seed_doc(project_id, DocumentKind::HumanLog, "stale");
        let never = seed_doc(project_id, DocumentKind::Interview, "never");
        for d in [&fresh, &stale, &never] {
            repo.apply_event(&EventEnvelope::new(
                Actor::user(),
                Event::DocumentCreated {
                    document: d.clone(),
                },
            ))
            .await
            .unwrap();
        }
        let throttle = std::time::Duration::from_secs(3600);
        let now = time::now();
        // `fresh` read just now, `stale` read long ago, `never` untouched.
        repo.mark_read(fresh.id, "user", now, throttle)
            .await
            .unwrap();
        repo.mark_read(stale.id, "user", now - chrono::Duration::days(30), throttle)
            .await
            .unwrap();

        // Unread for 7 days → stale + never, not fresh.
        let cutoff = now - chrono::Duration::days(7);
        let unread = repo.list_unread_since(project_id, cutoff).await.unwrap();
        let ids: Vec<_> = unread.iter().map(|d| d.id).collect();
        assert!(ids.contains(&stale.id), "stale doc should be unread");
        assert!(ids.contains(&never.id), "never-read doc should be unread");
        assert!(
            !ids.contains(&fresh.id),
            "freshly read doc should be excluded"
        );
    }

    #[tokio::test]
    async fn mark_read_unknown_id_is_noop() {
        let (_db, repo) = make_repo().await;
        let updated = repo
            .mark_read(
                DocumentId::new(),
                "user",
                time::now(),
                std::time::Duration::from_secs(3600),
            )
            .await
            .unwrap();
        assert!(!updated, "unknown document id must not count a read");
    }

    #[tokio::test]
    async fn mutate_unknown_id_is_noop() {
        let (_db, repo) = make_repo().await;
        // Apply replace to a non-existent id — must not error.
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::DocumentContentReplaced {
                document_id: DocumentId::new(),
                content: "ghost".to_string(),
                at: time::now(),
            },
        ))
        .await
        .unwrap();
    }
}
