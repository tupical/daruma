-- The selector belongs to the claim generation, not to its eventual releaser.
ALTER TABLE agent_claims ADD COLUMN holder_logical_agent_id TEXT;
