//! Document projection repository — materialises `Document*` events
//! into the `documents` SQLite table (PR1 §3-4).

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use taskagent_domain::{Document, DocumentKind};
use taskagent_events::{Event, EventEnvelope};
use taskagent_shared::{CoreError, DocumentId, ProjectId, Result};

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
            "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
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
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
                     FROM documents \
                     WHERE project_id = ? AND kind = ? \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .bind(kind.as_str())
            .fetch_all(&self.pool)
            .await,
            (Some(kind), false) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
                     FROM documents \
                     WHERE project_id = ? AND kind = ? AND archived_at IS NULL \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .bind(kind.as_str())
            .fetch_all(&self.pool)
            .await,
            (None, true) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
                     FROM documents \
                     WHERE project_id = ? \
                     ORDER BY created_at ASC, id ASC",
            )
            .bind(project_id.to_string())
            .fetch_all(&self.pool)
            .await,
            (None, false) => sqlx::query(
                "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
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
        sqlx::query(
            "INSERT OR REPLACE INTO documents \
             (id, project_id, kind, title, content, created_at, updated_at, archived_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(doc.id.to_string())
        .bind(doc.project_id.to_string())
        .bind(doc.kind.as_str())
        .bind(&doc.title)
        .bind(&doc.content)
        .bind(doc.created_at.to_rfc3339())
        .bind(doc.updated_at.to_rfc3339())
        .bind(doc.archived_at.as_ref().map(|t| t.to_rfc3339()))
        .execute(&mut **tx)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

async fn get_document_tx(
    tx: &mut Transaction<'_, Sqlite>,
    id: DocumentId,
) -> Result<Option<Document>> {
    let row = sqlx::query(
        "SELECT id, project_id, kind, title, content, created_at, updated_at, archived_at \
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

    let id = id_s
        .parse::<DocumentId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let project_id = project_id_s
        .parse::<ProjectId>()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let kind = parse_kind(&kind_s)?;
    let archived_at = archived_at_s.as_deref().map(parse_ts).transpose()?;

    Ok(Document {
        id,
        project_id,
        kind,
        title,
        content,
        created_at: parse_ts(&created_at_s)?,
        updated_at: parse_ts(&updated_at_s)?,
        archived_at,
    })
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
    use taskagent_domain::{Actor, NewDocument};
    use taskagent_events::EventEnvelope;
    use taskagent_shared::{time, DocumentId, ProjectId};

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
                plan_id: taskagent_shared::PlanId::new(),
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
