//! Lifecycle rule repository — projection over `lifecycle_rules`
//! (migration 0037), fed by `RuleCreated` / `RuleUpdated` / `RuleDisabled`
//! events. The event log stays the source of truth (spec invariant 6).
//!
//! Reads serve two callers: CRUD listing (HTTP/MCP) and the lifecycle gate,
//! which asks for the *effective* enabled rules of a scope chain + trigger.
//! `off`/`enabled=false` rules are not returned to the gate (invariant 2).

use crate::parse_ts;
use sqlx::{Row, SqlitePool};
use daruma_domain::{Condition, Requirement, Rule, RuleMode, RuleScope, RuleTrigger};
use daruma_events::{Event, EventEnvelope};
use daruma_shared::{CoreError, Result, RuleId};

pub struct RuleRepo {
    pool: SqlitePool,
}

impl RuleRepo {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // ── queries ────────────────────────────────────────────────────────────────

    pub async fn get(&self, id: RuleId) -> Result<Option<Rule>> {
        let row = sqlx::query(&select_sql("WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
        row.as_ref().map(row_to_rule).transpose()
    }

    /// All rules defined directly at a scope level (any enabled state), for
    /// CRUD listing. Ordered by `rule_key` for a stable view.
    pub async fn list_for_scope(&self, scope: &RuleScope) -> Result<Vec<Rule>> {
        let rows = match scope.id_string() {
            Some(id) => {
                sqlx::query(&select_sql(
                    "WHERE scope_kind = ? AND scope_id = ? ORDER BY rule_key",
                ))
                .bind(scope.kind())
                .bind(id)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&select_sql(
                    "WHERE scope_kind = ? AND scope_id IS NULL ORDER BY rule_key",
                ))
                .bind(scope.kind())
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| CoreError::storage(e.to_string()))?;
        rows.iter().map(row_to_rule).collect()
    }

    /// Effective enabled rules for a scope chain firing on `trigger`.
    ///
    /// `chain` lists the scopes from outermost (tenant) to innermost (task);
    /// the caller assembles it from the entity being gated. Returns only
    /// `enabled=true`, `mode != off` rules. Inheritance/override (spec §2) is
    /// resolved by `rule_key`: a rule defined at a *later* (inner) scope in
    /// `chain` wins over the same key at an outer scope — unless it *weakens*
    /// the rule (lowers `RuleMode::strictness`, including weakening to `off`)
    /// while the parent rule has `override_allowed = false`; a denied
    /// weakening leaves the parent effective (spec §2.3). Strengthening is
    /// always allowed. This is the hot path for the gate; when no scope in
    /// the chain has rows it is a single empty query.
    pub async fn effective_rules(
        &self,
        chain: &[RuleScope],
        trigger: RuleTrigger,
    ) -> Result<Vec<Rule>> {
        if chain.is_empty() {
            return Ok(vec![]);
        }
        let trigger_str = trigger_to_str(trigger);

        // Index of each scope in the chain so inner scopes override outer —
        // subject to the spec §2 weakening policy. `mode = 'off'` rows DO
        // participate here (an inner `off` is how a child disables an
        // inherited rule); `off` winners are dropped at the end so the gate
        // never evaluates them (spec invariant 2).
        let mut chosen: std::collections::HashMap<String, (usize, Rule)> =
            std::collections::HashMap::new();

        for (depth, scope) in chain.iter().enumerate() {
            let rows = match scope.id_string() {
                Some(id) => {
                    sqlx::query(&select_sql(
                        "WHERE scope_kind = ? AND scope_id = ? AND trigger = ? \
                         AND enabled = 1",
                    ))
                    .bind(scope.kind())
                    .bind(id)
                    .bind(trigger_str)
                    .fetch_all(&self.pool)
                    .await
                }
                None => {
                    sqlx::query(&select_sql(
                        "WHERE scope_kind = ? AND scope_id IS NULL AND trigger = ? \
                         AND enabled = 1",
                    ))
                    .bind(scope.kind())
                    .bind(trigger_str)
                    .fetch_all(&self.pool)
                    .await
                }
            }
            .map_err(|e| CoreError::storage(e.to_string()))?;

            for row in &rows {
                let rule = row_to_rule(row)?;
                match chosen.get(&rule.rule_key) {
                    // Same key at a same-or-deeper level already chosen —
                    // cannot happen for one (scope, key) thanks to the unique
                    // index; kept as a guard for malformed chains.
                    Some((d, _)) if *d >= depth => {}
                    // Inner scope overrides the same rule_key — unless it
                    // *weakens* (lowers strictness, incl. weakening to `off`)
                    // and the parent rule forbids override (spec §2.3): then
                    // the parent stays effective.
                    Some((_, incumbent)) => {
                        let weakens =
                            rule.mode.strictness() < incumbent.mode.strictness();
                        if weakens && !incumbent.override_allowed {
                            continue;
                        }
                        chosen.insert(rule.rule_key.clone(), (depth, rule));
                    }
                    None => {
                        chosen.insert(rule.rule_key.clone(), (depth, rule));
                    }
                }
            }
        }

        // Spec invariant 2: `off` is never evaluated — an effective `off`
        // winner means "no rule" (the child disabled the inherited one).
        let mut out: Vec<Rule> = chosen
            .into_values()
            .map(|(_, r)| r)
            .filter(|r| r.mode != RuleMode::Off)
            .collect();
        out.sort_by(|a, b| a.rule_key.cmp(&b.rule_key));
        Ok(out)
    }

    /// Apply a persisted rule event to the projection.
    pub async fn apply_event(&self, env: &EventEnvelope) -> Result<()> {
        match &env.payload {
            Event::RuleCreated { rule } | Event::RuleUpdated { rule } => self.upsert(rule).await,
            Event::RuleDisabled { rule_id, at } => {
                sqlx::query("UPDATE lifecycle_rules SET enabled = 0, updated_at = ? WHERE id = ?")
                    .bind(at.to_rfc3339())
                    .bind(rule_id.to_string())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| CoreError::storage(e.to_string()))?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn upsert(&self, rule: &Rule) -> Result<()> {
        let condition = rule
            .condition
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| CoreError::serde(e.to_string()))?;
        let requirement = serde_json::to_string(&rule.requirement)
            .map_err(|e| CoreError::serde(e.to_string()))?;

        sqlx::query(
            "INSERT INTO lifecycle_rules \
             (id, rule_key, title, scope_kind, scope_id, trigger, condition, requirement, \
              mode, message, override_allowed, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
              rule_key = excluded.rule_key, title = excluded.title, \
              condition = excluded.condition, requirement = excluded.requirement, \
              mode = excluded.mode, message = excluded.message, \
              override_allowed = excluded.override_allowed, enabled = excluded.enabled, \
              updated_at = excluded.updated_at",
        )
        .bind(rule.id.to_string())
        .bind(&rule.rule_key)
        .bind(&rule.title)
        .bind(rule.scope.kind())
        .bind(rule.scope.id_string())
        .bind(trigger_to_str(rule.trigger))
        .bind(condition)
        .bind(requirement)
        .bind(mode_to_str(rule.mode))
        .bind(&rule.message)
        .bind(rule.override_allowed as i64)
        .bind(rule.enabled as i64)
        .bind(rule.created_at.to_rfc3339())
        .bind(rule.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| CoreError::storage(e.to_string()))?;
        Ok(())
    }
}

fn select_sql(tail: &str) -> String {
    format!(
        "SELECT id, rule_key, title, scope_kind, scope_id, trigger, condition, requirement, \
         mode, message, override_allowed, enabled, created_at, updated_at \
         FROM lifecycle_rules {tail}"
    )
}

fn mode_to_str(mode: RuleMode) -> &'static str {
    match mode {
        RuleMode::Off => "off",
        RuleMode::Recommendation => "recommendation",
        RuleMode::Required => "required",
    }
}

fn trigger_to_str(trigger: RuleTrigger) -> &'static str {
    match trigger {
        RuleTrigger::ProjectCreated => "project.created",
        RuleTrigger::PlanCreated => "plan.created",
        RuleTrigger::PlanBeforeApprove => "plan.before_approve",
        RuleTrigger::TaskCreated => "task.created",
        RuleTrigger::TaskBeforeStart => "task.before_start",
        RuleTrigger::TaskBeforeComplete => "task.before_complete",
        RuleTrigger::RunBeforeExecute => "run.before_execute",
        RuleTrigger::RunBeforeComplete => "run.before_complete",
    }
}

fn row_to_rule(row: &sqlx::sqlite::SqliteRow) -> Result<Rule> {
    let id_str: String = row.try_get("id").map_err(map_row_err)?;
    let scope_kind: String = row.try_get("scope_kind").map_err(map_row_err)?;
    let scope_id: Option<String> = row.try_get("scope_id").map_err(map_row_err)?;
    let trigger_str: String = row.try_get("trigger").map_err(map_row_err)?;
    let condition_json: Option<String> = row.try_get("condition").map_err(map_row_err)?;
    let requirement_json: String = row.try_get("requirement").map_err(map_row_err)?;
    let mode_str: String = row.try_get("mode").map_err(map_row_err)?;
    let created_at: String = row.try_get("created_at").map_err(map_row_err)?;
    let updated_at: String = row.try_get("updated_at").map_err(map_row_err)?;

    let scope = parse_scope(&scope_kind, scope_id.as_deref())?;
    let trigger = parse_trigger(&trigger_str)?;
    let condition: Option<Condition> = condition_json
        .map(|j| serde_json::from_str(&j))
        .transpose()
        .map_err(|e| CoreError::serde(e.to_string()))?;
    let requirement: Requirement =
        serde_json::from_str(&requirement_json).map_err(|e| CoreError::serde(e.to_string()))?;

    Ok(Rule {
        id: id_str
            .parse()
            .map_err(|_| CoreError::storage("bad rule id"))?,
        rule_key: row.try_get("rule_key").map_err(map_row_err)?,
        title: row.try_get("title").map_err(map_row_err)?,
        scope,
        trigger,
        condition,
        requirement,
        mode: parse_mode(&mode_str)?,
        message: row.try_get("message").map_err(map_row_err)?,
        override_allowed: row
            .try_get::<i64, _>("override_allowed")
            .map_err(map_row_err)?
            != 0,
        enabled: row.try_get::<i64, _>("enabled").map_err(map_row_err)? != 0,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

fn map_row_err(e: sqlx::Error) -> CoreError {
    CoreError::storage(e.to_string())
}

fn parse_scope(kind: &str, id: Option<&str>) -> Result<RuleScope> {
    Ok(match kind {
        "tenant" => RuleScope::Tenant,
        "project" => RuleScope::Project {
            id: scope_id(id, "project")?,
        },
        "plan" => RuleScope::Plan {
            id: scope_id(id, "plan")?,
        },
        "task" => RuleScope::Task {
            id: scope_id(id, "task")?,
        },
        other => {
            return Err(CoreError::storage(format!(
                "unknown rule scope kind: {other}"
            )))
        }
    })
}

fn scope_id<T: std::str::FromStr>(id: Option<&str>, kind: &str) -> Result<T> {
    id.ok_or_else(|| CoreError::storage(format!("{kind} scope missing scope_id")))?
        .parse()
        .map_err(|_| CoreError::storage(format!("bad {kind} scope id")))
}

fn parse_trigger(s: &str) -> Result<RuleTrigger> {
    Ok(match s {
        "project.created" => RuleTrigger::ProjectCreated,
        "plan.created" => RuleTrigger::PlanCreated,
        "plan.before_approve" => RuleTrigger::PlanBeforeApprove,
        "task.created" => RuleTrigger::TaskCreated,
        "task.before_start" => RuleTrigger::TaskBeforeStart,
        "task.before_complete" => RuleTrigger::TaskBeforeComplete,
        "run.before_execute" => RuleTrigger::RunBeforeExecute,
        "run.before_complete" => RuleTrigger::RunBeforeComplete,
        other => return Err(CoreError::storage(format!("unknown rule trigger: {other}"))),
    })
}

fn parse_mode(s: &str) -> Result<RuleMode> {
    Ok(match s {
        "off" => RuleMode::Off,
        "recommendation" => RuleMode::Recommendation,
        "required" => RuleMode::Required,
        other => return Err(CoreError::storage(format!("unknown rule mode: {other}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    use daruma_domain::{Actor, NewRule};
    use daruma_shared::ProjectId;

    fn sample(scope: RuleScope, key: &str, mode: RuleMode) -> Rule {
        NewRule {
            id: None,
            rule_key: key.to_string(),
            title: "t".into(),
            scope,
            trigger: RuleTrigger::TaskBeforeComplete,
            condition: None,
            requirement: Requirement::CompletionNote {
                required_fields: vec!["actor".into()],
            },
            mode,
            message: "m".into(),
            override_allowed: true,
            enabled: true,
        }
        .into_rule(daruma_shared::time::now())
    }

    async fn repo() -> RuleRepo {
        let db = Db::memory().await.unwrap();
        db.migrate().await.unwrap();
        RuleRepo::new(db.pool().clone())
    }

    async fn apply(repo: &RuleRepo, ev: Event) {
        let env = EventEnvelope::new(Actor::user(), ev);
        repo.apply_event(&env).await.unwrap();
    }

    #[tokio::test]
    async fn create_get_update_roundtrip() {
        let repo = repo().await;
        let rule = sample(RuleScope::Tenant, "completion-note", RuleMode::Required);
        let id = rule.id;
        apply(&repo, Event::RuleCreated { rule: rule.clone() }).await;

        let fetched = repo.get(id).await.unwrap().unwrap();
        assert_eq!(fetched.rule_key, "completion-note");
        assert_eq!(fetched.mode, RuleMode::Required);

        let mut updated = fetched.clone();
        updated.mode = RuleMode::Recommendation;
        apply(&repo, Event::RuleUpdated { rule: updated }).await;
        assert_eq!(
            repo.get(id).await.unwrap().unwrap().mode,
            RuleMode::Recommendation
        );
    }

    #[tokio::test]
    async fn disabled_rule_excluded_from_effective() {
        let repo = repo().await;
        let rule = sample(RuleScope::Tenant, "completion-note", RuleMode::Required);
        let id = rule.id;
        apply(&repo, Event::RuleCreated { rule }).await;

        let chain = [RuleScope::Tenant];
        assert_eq!(
            repo.effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
                .await
                .unwrap()
                .len(),
            1
        );

        apply(
            &repo,
            Event::RuleDisabled {
                rule_id: id,
                at: daruma_shared::time::now(),
            },
        )
        .await;
        assert!(repo
            .effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn off_mode_not_returned_to_gate() {
        let repo = repo().await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(RuleScope::Tenant, "k", RuleMode::Off),
            },
        )
        .await;
        assert!(repo
            .effective_rules(&[RuleScope::Tenant], RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn inner_scope_overrides_same_rule_key() {
        let repo = repo().await;
        let project = ProjectId::new();
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(RuleScope::Tenant, "completion-note", RuleMode::Required),
            },
        )
        .await;
        let mut inner = sample(
            RuleScope::Project { id: project },
            "completion-note",
            RuleMode::Recommendation,
        );
        inner.message = "project override".into();
        apply(&repo, Event::RuleCreated { rule: inner }).await;

        let chain = [RuleScope::Tenant, RuleScope::Project { id: project }];
        let eff = repo
            .effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap();
        assert_eq!(eff.len(), 1, "same rule_key collapses to one");
        assert_eq!(eff[0].mode, RuleMode::Recommendation, "inner scope wins");
        assert_eq!(eff[0].message, "project override");
    }

    // ── Weakening policy (spec §2.3; OSS task 019eb65a-e5cd) ──────────────────

    /// tenant `required` with `override_allowed = false` cannot be weakened
    /// by a project-level `recommendation`: the parent stays effective.
    #[tokio::test]
    async fn weakening_denied_without_parent_override_allowed() {
        let repo = repo().await;
        let project = ProjectId::new();

        let mut parent = sample(RuleScope::Tenant, "completion-note", RuleMode::Required);
        parent.override_allowed = false;
        apply(&repo, Event::RuleCreated { rule: parent }).await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(
                    RuleScope::Project { id: project },
                    "completion-note",
                    RuleMode::Recommendation,
                ),
            },
        )
        .await;

        let chain = [RuleScope::Tenant, RuleScope::Project { id: project }];
        let eff = repo
            .effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap();
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].mode, RuleMode::Required, "weakening denied");
        assert_eq!(eff[0].scope, RuleScope::Tenant, "parent rule stays");
    }

    /// A child-level `off` disables an inherited rule — but only when the
    /// parent allows override; otherwise the parent survives.
    #[tokio::test]
    async fn child_off_follows_the_same_weakening_policy() {
        let repo = repo().await;
        let allowed_project = ProjectId::new();
        let denied_project = ProjectId::new();

        // Parent that allows weakening: child `off` removes the rule.
        let mut parent = sample(RuleScope::Tenant, "with-override", RuleMode::Required);
        parent.override_allowed = true;
        apply(&repo, Event::RuleCreated { rule: parent }).await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(
                    RuleScope::Project { id: allowed_project },
                    "with-override",
                    RuleMode::Off,
                ),
            },
        )
        .await;
        let eff = repo
            .effective_rules(
                &[RuleScope::Tenant, RuleScope::Project { id: allowed_project }],
                RuleTrigger::TaskBeforeComplete,
            )
            .await
            .unwrap();
        assert!(eff.is_empty(), "child off disables the inherited rule");

        // Parent that forbids weakening: child `off` is ignored.
        let mut strict = sample(RuleScope::Tenant, "no-override", RuleMode::Required);
        strict.override_allowed = false;
        apply(&repo, Event::RuleCreated { rule: strict }).await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(
                    RuleScope::Project { id: denied_project },
                    "no-override",
                    RuleMode::Off,
                ),
            },
        )
        .await;
        let eff = repo
            .effective_rules(
                &[RuleScope::Tenant, RuleScope::Project { id: denied_project }],
                RuleTrigger::TaskBeforeComplete,
            )
            .await
            .unwrap();
        let strict_eff: Vec<_> = eff.iter().filter(|r| r.rule_key == "no-override").collect();
        assert_eq!(strict_eff.len(), 1, "parent survives the denied off");
        assert_eq!(strict_eff[0].mode, RuleMode::Required);
    }

    /// Raising strictness is always allowed, regardless of the parent's
    /// `override_allowed` — and works across the full 4-level chain.
    #[tokio::test]
    async fn strengthening_always_allowed_across_chain() {
        let repo = repo().await;
        let project = ProjectId::new();
        let plan = daruma_shared::PlanId::new();
        let task = daruma_shared::TaskId::new();

        let mut parent = sample(RuleScope::Tenant, "completion-note", RuleMode::Recommendation);
        parent.override_allowed = false;
        apply(&repo, Event::RuleCreated { rule: parent }).await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(
                    RuleScope::Task { id: task },
                    "completion-note",
                    RuleMode::Required,
                ),
            },
        )
        .await;

        let chain = [
            RuleScope::Tenant,
            RuleScope::Project { id: project },
            RuleScope::Plan { id: plan },
            RuleScope::Task { id: task },
        ];
        let eff = repo
            .effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap();
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].mode, RuleMode::Required, "strengthening wins");
        assert_eq!(eff[0].scope, RuleScope::Task { id: task });
    }

    /// Denied weakening at one level does not stop a deeper level from
    /// legitimately strengthening again: tenant required (no override) →
    /// plan recommendation (denied) → task required (equal strictness, wins).
    #[tokio::test]
    async fn denied_weakening_then_deeper_equal_strictness_wins() {
        let repo = repo().await;
        let plan = daruma_shared::PlanId::new();
        let task = daruma_shared::TaskId::new();

        let mut parent = sample(RuleScope::Tenant, "completion-note", RuleMode::Required);
        parent.override_allowed = false;
        apply(&repo, Event::RuleCreated { rule: parent }).await;
        apply(
            &repo,
            Event::RuleCreated {
                rule: sample(
                    RuleScope::Plan { id: plan },
                    "completion-note",
                    RuleMode::Recommendation,
                ),
            },
        )
        .await;
        let mut task_rule = sample(
            RuleScope::Task { id: task },
            "completion-note",
            RuleMode::Required,
        );
        task_rule.message = "task-level".into();
        apply(&repo, Event::RuleCreated { rule: task_rule }).await;

        let chain = [
            RuleScope::Tenant,
            RuleScope::Plan { id: plan },
            RuleScope::Task { id: task },
        ];
        let eff = repo
            .effective_rules(&chain, RuleTrigger::TaskBeforeComplete)
            .await
            .unwrap();
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].mode, RuleMode::Required);
        assert_eq!(eff[0].message, "task-level", "deeper equal-strictness rule wins");
    }
}
