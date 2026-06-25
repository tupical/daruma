-- §3.8.8 — Optional `kind` discriminator for comments
-- (Intent | Progress | Outcome | Blocker | Research).
--
-- NULL = legacy/unclassified comment; backwards-compatible by design.
-- Validation of the enum string happens in the application layer
-- (daruma_domain::CommentKind::FromStr), not via a CHECK constraint —
-- this keeps the migration cheap and lets us evolve the variant set
-- without ALTERs.

ALTER TABLE comments ADD COLUMN kind TEXT NULL;
