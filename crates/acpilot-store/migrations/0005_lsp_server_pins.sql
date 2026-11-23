-- The language-server program a person approved, by content.
--
-- The question the agent pins answer, asked about a command a pack introduced: a
-- preset names a program. A program whose bytes changed under the same path is not
-- the server that was approved. Keyed by the program rather than by language because
-- two languages can share one program (`typescript-language-server` is both
-- `typescript` and `javascript`). Approving it twice would be two answers to one
-- question.
CREATE TABLE lsp_server_pins (
    program     TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    trusted_at  TEXT NOT NULL
);
