-- Binds a claim to the agent session (chat) that took it, so parallel chats
-- of one agent stay apart. NULL means no session was given; the owner is never
-- inferred from the agent's latest session.
ALTER TABLE agent_claims ADD COLUMN session_id TEXT;
