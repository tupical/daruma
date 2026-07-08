//! Agent capability profiles (P6) — a DERIVED projection over
//! `agent_capability_profiles` (migration 0044), mined from `WorkUnit*`
//! events. Rebuildable by event replay like every other projection.
//!
//! Scheduling consumes the profiles as a *preference* (drain ordering in
//! `work_unit_repo::try_claim_next`), never a hard binding: a unit whose
//! tags match nobody is still claimable by anyone, and a `user_set` row
//! always beats mining (`apply_event` refuses to touch it).
//!
//! Signals (advisory-mode MVP): the unit's `capability_tags` credit its
//! *holder* when the unit completes (1.0), is released unfinished (0.4), or
//! blocks (0.3). Scores fold in as an EWMA with a step of
//! `1 / min(evidence_count, 20)`, so early evidence moves the score fast and
//! a long history is stable. `confidence = n / (n + 5)`.

use daruma_events::{Event, EventEnvelope};
use daruma_shared::{AgentId, CoreError, Result, WorkUnitId};
use sqlx::{Row, SqlitePool};

/// One `(agent, capability)` row of the projection.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CapabilityProfile {
    pub agent_id: AgentId,
    pub capability: String,
    pub score: f64,
    pub confidence: f64,
    pub evidence_count: i64,
    pub last_observed_at: String,
    pub decay_half_life_days: f64,
    pub source: String,
}

pub struct CapabilityProfileRepo {
    pool: SqlitePool,
}

impl CapabilityProfileRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<CapabilityProfile>> {
        let rows = sqlx::query(
            "SELECT agent_id, capability, score, confidence, evidence_count, \
             last_observed_at, decay_half_life_days, source \
             FROM agent_capability_profiles WHERE agent_id = ? ORDER BY capability",
        )
        .bind(agent_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_profile).collect()
    }

    /// Explicit human override: fixed score, `source = 'user_set'`. Mining
    /// never overwrites it and the staleness cutoff does not apply — the
    /// user's word wins until they change it.
    pub async fn upsert_user_set(
        &self,
        agent_id: AgentId,
        capability: &str,
        score: f64,
        now: &str,
    ) -> Result<()> {
        let score = score.clamp(0.0, 1.0);
        sqlx::query(
            "INSERT INTO agent_capability_profiles \
             (agent_id, capability, score, confidence, evidence_count, last_observed_at, \
              source, updated_at) \
             VALUES (?, ?, ?, 1.0, 0, ?, 'user_set', ?) \
             ON CONFLICT(agent_id, capability) DO UPDATE SET \
                score = excluded.score, confidence = 1.0, source = 'user_set', \
                last_observed_at = excluded.last_observed_at, updated_at = excluded.updated_at",
        )
        .bind(agent_id.to_string())
        .bind(capability)
        .bind(score)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }

    /// Remove a profile row (e.g. retract a user override; mining will
    /// re-derive an inferred row from future evidence).
    pub async fn delete(&self, agent_id: AgentId, capability: &str) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM agent_capability_profiles WHERE agent_id = ? AND capability = ?",
        )
        .bind(agent_id.to_string())
        .bind(capability)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    /// Apply a persisted event. MUST run *before* `WorkUnitRepo::apply_event`
    /// in the projection chain: completion/release clear the unit's
    /// `owner_agent_id`, and the owner is exactly who the signal credits.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        let (unit_id, signal) = match &env.payload {
            Event::WorkUnitCompleted { work_unit_id, .. } => (*work_unit_id, 1.0_f64),
            Event::WorkUnitReleased { work_unit_id, .. } => (*work_unit_id, 0.4),
            Event::WorkUnitBlocked { work_unit_id, .. } => (*work_unit_id, 0.3),
            _ => return Ok(()),
        };
        let Some((owner, tags)) = self.unit_owner_and_tags(unit_id).await? else {
            return Ok(()); // unknown unit or nobody holds it — no signal
        };
        let now = env.occurred_at.to_rfc3339();
        for capability in tags {
            self.fold_signal(&owner, &capability, signal, &now).await?;
        }
        Ok(())
    }

    async fn unit_owner_and_tags(&self, id: WorkUnitId) -> Result<Option<(String, Vec<String>)>> {
        let row =
            sqlx::query("SELECT owner_agent_id, capability_tags_json FROM work_units WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| CoreError::storage(e.to_string()))?;
        let Some(row) = row else { return Ok(None) };
        let owner: Option<String> = row
            .try_get("owner_agent_id")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let Some(owner) = owner else { return Ok(None) };
        let tags_s: String = row
            .try_get("capability_tags_json")
            .map_err(|e| CoreError::storage(e.to_string()))?;
        let tags: Vec<String> =
            serde_json::from_str(&tags_s).map_err(|e| CoreError::serde(e.to_string()))?;
        if tags.is_empty() {
            return Ok(None);
        }
        Ok(Some((owner, tags)))
    }

    /// EWMA fold of one signal into an `(agent, capability)` row.
    /// `user_set` rows are immune — the WHERE clause skips them.
    async fn fold_signal(
        &self,
        agent_id: &str,
        capability: &str,
        signal: f64,
        now: &str,
    ) -> Result<()> {
        // Insert-or-fold in one statement: new rows start AT the signal
        // (n=1, step=1); existing inferred rows move by 1/min(n+1, 20).
        sqlx::query(
            "INSERT INTO agent_capability_profiles \
             (agent_id, capability, score, confidence, evidence_count, last_observed_at, \
              source, updated_at) \
             VALUES (?, ?, ?, 1.0/6.0, 1, ?, 'inferred', ?) \
             ON CONFLICT(agent_id, capability) DO UPDATE SET \
                score = score + (excluded.score - score) / MIN(evidence_count + 1, 20), \
                evidence_count = evidence_count + 1, \
                confidence = CAST(evidence_count + 1 AS REAL) / (evidence_count + 6), \
                last_observed_at = excluded.last_observed_at, \
                updated_at = excluded.updated_at \
             WHERE source != 'user_set'",
        )
        .bind(agent_id)
        .bind(capability)
        .bind(signal)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

fn row_to_profile(row: &sqlx::sqlite::SqliteRow) -> Result<CapabilityProfile> {
    fn col<T>(v: std::result::Result<T, sqlx::Error>) -> Result<T> {
        v.map_err(|e| CoreError::storage(e.to_string()))
    }
    let agent_s: String = col(row.try_get("agent_id"))?;
    Ok(CapabilityProfile {
        agent_id: agent_s
            .parse::<AgentId>()
            .map_err(|e| CoreError::serde(e.to_string()))?,
        capability: col(row.try_get("capability"))?,
        score: col(row.try_get("score"))?,
        confidence: col(row.try_get("confidence"))?,
        evidence_count: col(row.try_get("evidence_count"))?,
        last_observed_at: col(row.try_get("last_observed_at"))?,
        decay_half_life_days: col(row.try_get("decay_half_life_days"))?,
        source: col(row.try_get("source"))?,
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, WorkUnitRepo};
    use daruma_domain::{Actor, WorkUnit};
    use daruma_shared::time;

    async fn stack() -> (Db, WorkUnitRepo, CapabilityProfileRepo) {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        let wu = WorkUnitRepo::new(db.pool().clone());
        let prof = CapabilityProfileRepo::new(db.pool().clone());
        (db, wu, prof)
    }

    async fn seed_claimed_unit(wu: &WorkUnitRepo, agent: AgentId, tags: &[&str]) -> WorkUnitId {
        let mut unit = WorkUnit::sample(daruma_shared::TaskId::new());
        unit.capability_tags = tags.iter().map(|s| s.to_string()).collect();
        unit.owner_agent_id = Some(agent);
        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCreated {
                work_unit: unit.clone(),
            },
        );
        wu.apply_event(&env).await.unwrap();
        unit.id
    }

    fn completed(id: WorkUnitId) -> EventEnvelope {
        EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCompleted {
                work_unit_id: id,
                outcome: "ok".into(),
                produced_artifacts: vec![],
                next_suggested_units: vec![],
                at: time::now(),
            },
        )
    }

    #[tokio::test]
    async fn completion_credits_the_holder_per_tag() {
        let (_db, wu, prof) = stack().await;
        let agent = AgentId::new();
        let unit = seed_claimed_unit(&wu, agent, &["frontend", "tests"]).await;

        prof.apply_event(&completed(unit)).await.unwrap();

        let rows = prof.list_for_agent(agent).await.unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.score, 1.0, "first signal lands at its value");
            assert_eq!(r.evidence_count, 1);
            assert_eq!(r.source, "inferred");
        }

        // A blocked signal on a second unit pulls the score down by the
        // EWMA step (n=2 → step 1/2): 1.0 + (0.3 - 1.0)/2 = 0.65.
        let unit2 = seed_claimed_unit(&wu, agent, &["frontend"]).await;
        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitBlocked {
                work_unit_id: unit2,
                reason: "stuck".into(),
                at: time::now(),
            },
        );
        prof.apply_event(&env).await.unwrap();
        let rows = prof.list_for_agent(agent).await.unwrap();
        let frontend = rows.iter().find(|r| r.capability == "frontend").unwrap();
        assert!(
            (frontend.score - 0.65).abs() < 1e-9,
            "got {}",
            frontend.score
        );
        assert_eq!(frontend.evidence_count, 2);
    }

    #[tokio::test]
    async fn user_set_rows_are_immune_to_mining_and_win() {
        let (_db, wu, prof) = stack().await;
        let agent = AgentId::new();
        prof.upsert_user_set(agent, "db", 0.9, &time::now().to_rfc3339())
            .await
            .unwrap();

        let unit = seed_claimed_unit(&wu, agent, &["db"]).await;
        // A weak signal must not move the user-set score.
        let env = EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitBlocked {
                work_unit_id: unit,
                reason: "stuck".into(),
                at: time::now(),
            },
        );
        prof.apply_event(&env).await.unwrap();

        let rows = prof.list_for_agent(agent).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "user_set");
        assert_eq!(rows[0].score, 0.9);

        // Retract → next mined signal re-derives an inferred row.
        assert!(prof.delete(agent, "db").await.unwrap());
        prof.apply_event(&completed(unit)).await.unwrap();
        let rows = prof.list_for_agent(agent).await.unwrap();
        assert_eq!(rows[0].source, "inferred");
    }

    #[tokio::test]
    async fn unowned_or_untagged_units_emit_no_signal() {
        let (_db, wu, prof) = stack().await;
        let agent = AgentId::new();

        // Tagged but unowned.
        let mut unit = WorkUnit::sample(daruma_shared::TaskId::new());
        unit.capability_tags = vec!["frontend".into()];
        wu.apply_event(&EventEnvelope::new(
            Actor::user(),
            Event::WorkUnitCreated {
                work_unit: unit.clone(),
            },
        ))
        .await
        .unwrap();
        prof.apply_event(&completed(unit.id)).await.unwrap();

        // Owned but untagged.
        let untagged = seed_claimed_unit(&wu, agent, &[]).await;
        prof.apply_event(&completed(untagged)).await.unwrap();

        assert!(prof.list_for_agent(agent).await.unwrap().is_empty());
    }
}
