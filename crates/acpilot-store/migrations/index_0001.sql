-- The small global index: which profiles exist and where they have worked.
--
-- It is deliberately not a conversation store. A profile's sessions, events and
-- decisions live in that profile's `profile.sqlite`; this file only answers "which
-- directories are profiles" and "what workspaces has this profile seen" without
-- opening every profile database. `first_seen` is an audit timestamp, not an
-- ordering key: rows are listed by rowid, because a `jiff` timestamp's fractional
-- width makes text order disagree with time order.

CREATE TABLE profiles (
    id         TEXT PRIMARY KEY,
    first_seen TEXT NOT NULL
);

CREATE TABLE workspaces (
    profile    TEXT NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    first_seen TEXT NOT NULL,
    PRIMARY KEY (profile, path)
);

CREATE INDEX workspaces_by_profile ON workspaces (profile);
