-- The profile a session belongs to.
--
-- The database already lives inside one profile's data directory, so this is
-- redundant for isolation and deliberately so: a session summary read after a
-- restart has to name its owner without asking the process that opened it. Rows
-- from before P4-6 read as the empty profile, which is what a daemon started
-- without a profile means.
ALTER TABLE sessions ADD COLUMN profile TEXT NOT NULL DEFAULT '';
