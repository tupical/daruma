-- Rebuildable reference snapshots; the source remains authoritative.
CREATE TABLE audit_source_snapshots (
    source TEXT NOT NULL,
    scope TEXT NOT NULL,
    ref TEXT NOT NULL,
    revision TEXT NOT NULL,
    scan_id TEXT NOT NULL,
    PRIMARY KEY (source, scope, ref)
);

CREATE TABLE audit_source_cursors (
    source TEXT NOT NULL,
    scope TEXT NOT NULL,
    cursor TEXT NOT NULL,
    scan_id TEXT NOT NULL,
    complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
    PRIMARY KEY (source, scope)
);
