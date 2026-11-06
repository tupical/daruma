-- Stable identity of this server's event log (crates/storage/src/server_identity.rs).
--
-- A seq only means something inside one log, so a device tells logs apart by
-- this value rather than by URL or token. It lives in the same database as the
-- log: it survives restarts, new addresses and new certificates, and a fresh
-- database gets a new one.
CREATE TABLE server_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    server_id TEXT NOT NULL
);

INSERT INTO server_identity (id, server_id) VALUES (1, lower(hex(randomblob(16))));
