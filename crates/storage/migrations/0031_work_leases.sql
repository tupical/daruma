-- Work leases: lightweight, TTL'd file/path reservations so multiple agents
-- closing tasks in parallel never edit the same files. "Like git, but lighter":
-- a reservation records the path globs an agent is touching and is swept on
-- task completion or expiry. Exclusivity (overlap detection) is enforced in
-- the repo layer via a single-statement transaction.

CREATE TABLE IF NOT EXISTS work_leases (
    id          TEXT    NOT NULL PRIMARY KEY,
    agent_id    TEXT    NOT NULL,
    task_id     TEXT    NOT NULL,
    project_id  TEXT,
    path_glob   TEXT    NOT NULL,   -- normalized, repo-relative prefix/glob
    acquired_at TEXT    NOT NULL,
    expires_at  TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_work_leases_task    ON work_leases (task_id);
CREATE INDEX IF NOT EXISTS idx_work_leases_project ON work_leases (project_id);
CREATE INDEX IF NOT EXISTS idx_work_leases_expires ON work_leases (expires_at);
CREATE INDEX IF NOT EXISTS idx_work_leases_agent   ON work_leases (agent_id);
