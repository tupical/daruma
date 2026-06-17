-- Lifecycle rules (docs/LIFECYCLE_RULES_SPEC.md §4).
-- Columnar projection of RuleCreated/RuleUpdated/RuleDisabled events; the
-- event log remains the source of truth. The lifecycle gate reads effective
-- rules from this table on selected pre-transition points.

CREATE TABLE IF NOT EXISTS lifecycle_rules (
    id          TEXT PRIMARY KEY,
    -- Stable key for inheritance / override across scope levels.
    rule_key    TEXT NOT NULL,
    title       TEXT NOT NULL DEFAULT '',
    -- Scope: 'tenant' | 'project' | 'plan' | 'task'. scope_id is NULL for tenant.
    scope_kind  TEXT NOT NULL,
    scope_id    TEXT,
    -- Trigger wire string, e.g. 'task.before_complete'.
    trigger     TEXT NOT NULL,
    -- JSON Condition ('{}'-style object) or NULL = fires on every trigger.
    condition   TEXT,
    -- JSON Requirement (tagged by "type").
    requirement TEXT NOT NULL,
    -- 'off' | 'recommendation' | 'required'.
    mode        TEXT NOT NULL DEFAULT 'off',
    message     TEXT NOT NULL DEFAULT '',
    override_allowed INTEGER NOT NULL DEFAULT 0,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- One rule_key per scope level (spec §2: conflicts on one level are rejected).
-- scope_id is the empty string for tenant so the unique index is well-defined.
CREATE UNIQUE INDEX IF NOT EXISTS idx_lifecycle_rules_scope_key
    ON lifecycle_rules(scope_kind, COALESCE(scope_id, ''), rule_key);

-- Hot-path lookup: enabled rules for a (scope, trigger) pair.
CREATE INDEX IF NOT EXISTS idx_lifecycle_rules_scope_trigger
    ON lifecycle_rules(scope_kind, scope_id, trigger, enabled);
