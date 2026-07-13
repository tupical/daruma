-- P6 slice 2 (event payload enrichment for process-mining): outcome/quality
-- facts recorded on terminal/transition events (WorkUnitCompleted,
-- HandoffAccepted/Rejected) are persisted here so upper layers can mine them
-- with plain SQL over the projections, not only by replaying the event log.
--
-- All columns are nullable with no default: existing rows read back NULL and
-- events persisted before the enrichment (which omit the fields) project to
-- NULL as well. Nothing here changes current-state semantics — these are
-- write-through facts for mining, not read back into the domain structs.

-- Who closed the unit (holder at completion) and its cycle time in ms.
ALTER TABLE work_units ADD COLUMN completed_by TEXT;
ALTER TABLE work_units ADD COLUMN elapsed_ms   INTEGER;

-- Handoff response latency in ms (request → accept/reject). Reset to NULL when
-- a rejected contract is re-requested (reopened), since no response is pending.
ALTER TABLE handoff_contracts ADD COLUMN latency_ms INTEGER;
