-- Legacy and offline evidence has no authenticated provenance. Never backfill from actor_id.
ALTER TABLE evidence ADD COLUMN authenticated_actor_id TEXT;
