-- The first schema: what a daemon needs to answer "what happened" after a restart.
--
-- Every table here is a record of something the daemon already knew in memory.
-- Nothing in the request path reads it before writing: the database is the
-- history, not the live state, so a slow disk costs a log line rather than a turn.

CREATE TABLE workspaces (
    id         TEXT PRIMARY KEY,
    root       TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE hosts (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('local', 'ssh', 'wsl')),
    address    TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE agents (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    transport  TEXT NOT NULL,
    command    TEXT,
    seen_at    TEXT NOT NULL
);

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    agent_id     TEXT NOT NULL,
    title        TEXT NOT NULL,
    status       TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

-- `seq` is the session's own monotonic sequence, so the primary key is what makes
-- a re-delivered event an update rather than a duplicate: the replay ring and the
-- database must agree about a sequence's contents.
CREATE TABLE events (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    at         TEXT NOT NULL,
    at_unix_ms INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

-- Retention deletes by this column; without the index it is a full scan of every
-- event the profile ever produced, once per sweep.
CREATE INDEX events_by_time ON events (at_unix_ms);

CREATE TABLE grants (
    session_id TEXT NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    at         TEXT NOT NULL,
    at_unix_ms INTEGER NOT NULL,
    tool       TEXT NOT NULL,
    decision   TEXT NOT NULL,
    decided_by TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

CREATE INDEX grants_by_time ON grants (at_unix_ms);

CREATE TABLE terminals (
    id         TEXT PRIMARY KEY,
    session_id TEXT REFERENCES sessions (id) ON DELETE CASCADE,
    program    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    ended_at   TEXT
);
