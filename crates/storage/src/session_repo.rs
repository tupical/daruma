//! AgentSession projection repository — materialises session-related events
//! into the `agent_sessions` SQLite table. Linear B.1: stores plan_steps_json.

use crate::parse_ts;
use sqlx::{Row, SqlitePool};
use daruma_domain::{AgentSession, AgentSessionPlanStep, SessionArtifact, SessionArtifactKind};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, AgentSessionId, CoreError, Result, SessionArtifactId, Timestamp};

/// Read/write access to the `agent_sessions` projection table.
pub struct SessionRepo {
    pub(crate) pool: SqlitePool,
}

impl SessionRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    pub async fn get(&self, id: AgentSessionId) -> Result<Option<AgentSession>> {
        let row = sqlx::query(
            "SELECT id, agent_id, parent_agent_id, started_at, ended_at, \
             metadata_json, plan_steps_json FROM agent_sessions WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        row.as_ref().map(row_to_session).transpose()
    }

    pub async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<AgentSession>> {
        let rows = sqlx::query(
            "SELECT id, agent_id, parent_agent_id, started_at, ended_at, \
             metadata_json, plan_steps_json \
             FROM agent_sessions WHERE agent_id = ? ORDER BY started_at ASC",
        )
        .bind(agent_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_session).collect()
    }

    pub async fn list_artifacts(&self, session_id: AgentSessionId) -> Result<Vec<SessionArtifact>> {
        let rows = sqlx::query(
            "SELECT id, session_id, kind, ref, metadata_json, created_at \
             FROM session_artifacts WHERE session_id = ? \
             ORDER BY created_at ASC, id ASC",
        )
        .bind(session_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        rows.iter().map(row_to_artifact).collect()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    pub async fn start(&self, session: &AgentSession) -> Result<()> {
        self.upsert_session(session).await
    }

    pub async fn end(&self, session_id: AgentSessionId, at: Timestamp) -> Result<()> {
        sqlx::query("UPDATE agent_sessions SET ended_at = ? WHERE id = ?")
            .bind(at.to_rfc3339())
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Replace the session's plan steps in full (Linear B.1).
    pub async fn update_plan_steps(
        &self,
        session_id: AgentSessionId,
        steps: &[AgentSessionPlanStep],
    ) -> Result<()> {
        let steps_json =
            serde_json::to_string(steps).map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query("UPDATE agent_sessions SET plan_steps_json = ? WHERE id = ?")
            .bind(steps_json)
            .bind(session_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    pub async fn upsert_artifact(&self, artifact: &SessionArtifact) -> Result<()> {
        let metadata_json = serde_json::to_string(&artifact.metadata)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        sqlx::query(
            "INSERT OR REPLACE INTO session_artifacts \
             (id, session_id, kind, ref, metadata_json, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(artifact.id.to_string())
        .bind(artifact.session_id.to_string())
        .bind(artifact_kind_str(&artifact.kind))
        .bind(&artifact.reference)
        .bind(metadata_json)
        .bind(artifact.created_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    // ── event application ────────────────────────────────────────────────────

    pub async fn apply_event(&self, envelope: &EventEnvelope) -> Result<()> {
        match &envelope.payload {
            Event::AgentSessionStarted { session } => {
                self.upsert_session(session).await?;
            }

            Event::AgentSessionEnded { session_id, at } => {
                self.end(*session_id, *at).await?;
            }

            Event::AgentSessionPlanUpdated { session_id, steps } => {
                self.update_plan_steps(*session_id, steps).await?;
            }

            Event::SessionArtifactAttached { artifact } => {
                self.upsert_artifact(artifact).await?;
            }

            _ => {}
        }

        Ok(())
    }

    // ── private helpers ──────────────────────────────────────────────────────

    async fn upsert_session(&self, session: &AgentSession) -> Result<()> {
        let parent_agent_id = session.parent_agent_id.map(|a| a.to_string());
        let ended_at = session.ended_at.map(|t| t.to_rfc3339());
        let metadata_json = serde_json::to_string(&session.metadata)
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let plan_steps_json = serde_json::to_string(&session.plan_steps)
            .map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT OR REPLACE INTO agent_sessions \
             (id, agent_id, parent_agent_id, started_at, ended_at, \
              metadata_json, plan_steps_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session.id.to_string())
        .bind(session.agent_id.to_string())
        .bind(parent_agent_id)
        .bind(session.started_at.to_rfc3339())
        .bind(ended_at)
        .bind(metadata_json)
        .bind(plan_steps_json)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;

        Ok(())
    }
}

// ── row mapping ───────────────────────────────────────────────────────────────

fn row_to_session(row: &sqlx::sqlite::SqliteRow) -> Result<AgentSession> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let agent_id: String = row
        .try_get("agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let parent_agent_id: Option<String> = row
        .try_get("parent_agent_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let started_at_s: String = row
        .try_get("started_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let ended_at_s: Option<String> = row
        .try_get("ended_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let metadata_json: String = row
        .try_get("metadata_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let plan_steps_json: String = row
        .try_get("plan_steps_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(AgentSession {
        id: id
            .parse::<AgentSessionId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        agent_id: agent_id
            .parse::<AgentId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        parent_agent_id: parent_agent_id
            .map(|s| {
                s.parse::<AgentId>()
                    .map_err(|e| CoreError::serde(e.to_string()))
            })
            .transpose()?,
        started_at: parse_ts(&started_at_s)?,
        ended_at: ended_at_s.map(|s| parse_ts(&s)).transpose()?,
        plan_steps: serde_json::from_str(&plan_steps_json)
            .map_err(|e| CoreError::serde(e.to_string()))?,
        metadata: serde_json::from_str(&metadata_json)
            .map_err(|e| CoreError::serde(e.to_string()))?,
    })
}

fn artifact_kind_str(kind: &SessionArtifactKind) -> &'static str {
    match kind {
        SessionArtifactKind::File => "file",
        SessionArtifactKind::Url => "url",
        SessionArtifactKind::Diff => "diff",
    }
}

fn parse_artifact_kind(raw: &str) -> Result<SessionArtifactKind> {
    match raw {
        "file" => Ok(SessionArtifactKind::File),
        "url" => Ok(SessionArtifactKind::Url),
        "diff" => Ok(SessionArtifactKind::Diff),
        other => Err(CoreError::serde(format!(
            "unknown session artifact kind: {other}"
        ))),
    }
}

fn row_to_artifact(row: &sqlx::sqlite::SqliteRow) -> Result<SessionArtifact> {
    let id: String = row
        .try_get("id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let session_id: String = row
        .try_get("session_id")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let kind: String = row
        .try_get("kind")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let reference: String = row
        .try_get("ref")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let metadata_json: String = row
        .try_get("metadata_json")
        .map_err(|e| CoreError::storage(e.to_string()))?;
    let created_at_s: String = row
        .try_get("created_at")
        .map_err(|e| CoreError::storage(e.to_string()))?;

    Ok(SessionArtifact {
        id: id
            .parse::<SessionArtifactId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        session_id: session_id
            .parse::<AgentSessionId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        kind: parse_artifact_kind(&kind)?,
        reference,
        metadata: serde_json::from_str(&metadata_json)
            .map_err(|e| CoreError::serde(e.to_string()))?,
        created_at: parse_ts(&created_at_s)?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, SessionArtifactKind, SessionStepStatus};
    use daruma_events::{Event, EventEnvelope};
    use daruma_shared::{time, AgentId, AgentSessionId, SessionArtifactId};

    async fn make_repo() -> (Db, SessionRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let repo = SessionRepo::new(db.pool().clone());
        (db, repo)
    }

    fn make_session(id: AgentSessionId, agent_id: AgentId) -> AgentSession {
        AgentSession {
            id,
            agent_id,
            parent_agent_id: None,
            started_at: time::now(),
            ended_at: None,
            plan_steps: vec![],
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn session_start_and_get() {
        let (_db, repo) = make_repo().await;
        let session_id = AgentSessionId::new();
        let agent_id = AgentId::new();
        let session = make_session(session_id, agent_id);

        repo.start(&session).await.unwrap();

        let fetched = repo.get(session_id).await.unwrap().expect("session exists");
        assert_eq!(fetched.id, session_id);
        assert_eq!(fetched.agent_id, agent_id);
        assert!(fetched.ended_at.is_none());
    }

    #[tokio::test]
    async fn session_list_for_agent() {
        let (_db, repo) = make_repo().await;
        let agent_id = AgentId::new();
        let other_agent = AgentId::new();

        repo.start(&make_session(AgentSessionId::new(), agent_id))
            .await
            .unwrap();
        repo.start(&make_session(AgentSessionId::new(), agent_id))
            .await
            .unwrap();
        repo.start(&make_session(AgentSessionId::new(), other_agent))
            .await
            .unwrap();

        let mine = repo.list_for_agent(agent_id).await.unwrap();
        assert_eq!(mine.len(), 2);

        let others = repo.list_for_agent(other_agent).await.unwrap();
        assert_eq!(others.len(), 1);
    }

    #[tokio::test]
    async fn session_update_plan_steps() {
        let (_db, repo) = make_repo().await;
        let session_id = AgentSessionId::new();
        let agent_id = AgentId::new();
        repo.start(&make_session(session_id, agent_id))
            .await
            .unwrap();

        let steps = vec![
            AgentSessionPlanStep {
                content: "step 1".to_string(),
                status: SessionStepStatus::InProgress,
            },
            AgentSessionPlanStep {
                content: "step 2".to_string(),
                status: SessionStepStatus::Pending,
            },
        ];

        repo.update_plan_steps(session_id, &steps).await.unwrap();

        let fetched = repo.get(session_id).await.unwrap().unwrap();
        assert_eq!(fetched.plan_steps.len(), 2);
        assert_eq!(fetched.plan_steps[0].content, "step 1");
        assert_eq!(fetched.plan_steps[0].status, SessionStepStatus::InProgress);
    }

    #[tokio::test]
    async fn session_apply_event_started_and_ended() {
        let (_db, repo) = make_repo().await;
        let session_id = AgentSessionId::new();
        let agent_id = AgentId::new();
        let session = make_session(session_id, agent_id);

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::AgentSessionStarted { session },
        ))
        .await
        .unwrap();

        let fetched = repo.get(session_id).await.unwrap().expect("session exists");
        assert!(fetched.ended_at.is_none());

        let at = time::now();
        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::AgentSessionEnded { session_id, at },
        ))
        .await
        .unwrap();

        let fetched2 = repo.get(session_id).await.unwrap().unwrap();
        assert!(fetched2.ended_at.is_some());
    }

    #[tokio::test]
    async fn session_apply_event_plan_updated() {
        let (_db, repo) = make_repo().await;
        let session_id = AgentSessionId::new();
        let agent_id = AgentId::new();
        repo.start(&make_session(session_id, agent_id))
            .await
            .unwrap();

        let steps = vec![AgentSessionPlanStep {
            content: "do the thing".to_string(),
            status: SessionStepStatus::Pending,
        }];

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::AgentSessionPlanUpdated {
                session_id,
                steps: steps.clone(),
            },
        ))
        .await
        .unwrap();

        let fetched = repo.get(session_id).await.unwrap().unwrap();
        assert_eq!(fetched.plan_steps.len(), 1);
        assert_eq!(fetched.plan_steps[0].content, "do the thing");
    }

    #[tokio::test]
    async fn session_artifact_apply_event_and_list() {
        let (_db, repo) = make_repo().await;
        let session_id = AgentSessionId::new();
        let agent_id = AgentId::new();
        repo.start(&make_session(session_id, agent_id))
            .await
            .unwrap();

        let artifact = SessionArtifact {
            id: SessionArtifactId::new(),
            session_id,
            kind: SessionArtifactKind::File,
            reference: "target/report.txt".into(),
            metadata: serde_json::json!({"bytes": 42}),
            created_at: time::now(),
        };

        repo.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::SessionArtifactAttached {
                artifact: artifact.clone(),
            },
        ))
        .await
        .unwrap();

        let artifacts = repo.list_artifacts(session_id).await.unwrap();
        assert_eq!(artifacts, vec![artifact]);
    }
}
