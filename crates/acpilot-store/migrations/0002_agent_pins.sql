-- The binary a person approved, by content.
--
-- A pin is only meaningful against a fingerprint of the file that ran: a row that
-- recorded the command instead would call a replaced binary the same agent, which is
-- the one thing `AgentStatus::Untrusted` exists to notice. `trusted_at` is when a
-- person looked, which is what an audit reads, and it is rewritten when the pin is.
CREATE TABLE agent_pins (
    agent_id    TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    trusted_at  TEXT NOT NULL
);
