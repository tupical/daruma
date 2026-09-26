-- Plan source umbrella (ADR-0009): normalised URI of where the work came from
-- (issue, e-mail, self://…). NULL for plans created before the column; not
-- unique — one source may produce many plans.
ALTER TABLE plans ADD COLUMN source_ref TEXT;
CREATE INDEX idx_plans_source_ref ON plans(project_id, source_ref);
