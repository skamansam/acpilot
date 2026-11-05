//! The daemon's durable record: one SQLite database per profile.
//!
//! Everything the daemon keeps in memory is rebuilt on a restart except the
//! conversations people had. This crate is what keeps those: a `sessions` row per
//! conversation, an `events` row per sequence, a `grants` row per permission
//! decision, and the terminals that carried them. The live state stays where it
//! is — the replay ring, the broker, the terminal registry — because a database
//! read on the request path would trade a fast answer for a durable one.
//!
//! # One database per profile
//!
//! Profiles are isolation boundaries (the profiles phase, P4), so a shared database
//! would put a client's conversation beside a weekend project's. The path is the
//! caller's to name and the daemon asks its profile for it; [`Store::open`] creates
//! the file and the directory around it, so a first run is not a special case.
//!
//! # Migrations are idempotent
//!
//! [`MIGRATIONS`] runs on every open and does nothing when the database is already
//! current: `rusqlite_migration` keeps `PRAGMA user_version`, so a profile opened by
//! a newer daemon and then by an older one is refused rather than half-migrated.
//! That is what makes "open the store" one call instead of a version check each
//! caller repeats.
//!
//! `WAL` is on and `foreign_keys` is on per connection, not per database: SQLite
//! defaults both off, and a foreign key that is only foreign when someone
//! remembered is worse than none.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_proto::event::SessionEvent;
use acpilot_proto::host::HostKind;
use acpilot_proto::session::{SessionStatus, SessionSummary};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

/// The database file's name inside a profile's data directory.
pub const FILE_NAME: &str = "acpilot.db";

/// How long an event is kept before [`Store::compact`] drops it.
///
/// Thirty days is the service phase's task P3-7 number. Compaction is a deletion
/// rather than a roll-up because the append-only transcript beside the database is
/// the record that outlives it: the database is the queryable window, not the only
/// copy.
pub const RETENTION_DAYS: i64 = 30;

/// The schema this build expects.
///
/// Bumped with every entry in [`MIGRATIONS`]. A database written by a newer daemon
/// is refused rather than read: the columns a newer build added are the ones this
/// one would try to write past.
pub const SCHEMA_VERSION: u32 = 2;

/// The schema, in order, as `(version, sql)`.
///
/// Hand-rolled rather than `rusqlite_migration`: that crate links its own
/// `sqlite3`, which conflicts with the `rusqlite` the workspace pins, and the
/// runner is `PRAGMA user_version` and a transaction — smaller than the
/// workaround. A new migration is one more entry here and one more file in
/// `migrations/`; an edited migration that has shipped is a database that
/// disagrees with itself.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_agent_pins.sql")),
];

/// A profile's conversations, sessions and decisions.
///
/// `Clone` is cheap and shares one connection: the daemon hands it to every route
/// and to each session's owner task, and SQLite is the thing that serialises the
/// writes. One connection rather than a pool because the database is a history,
/// not a hot path — a pooled write would add a lock nobody is waiting on.
#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open the database at `path`, creating it and its directory if needed.
    ///
    /// A store that cannot be opened is an error the caller has to decide about:
    /// a daemon that silently ran without one would answer "no sessions" to a
    /// client asking about a conversation it had yesterday.
    ///
    /// # Errors
    ///
    /// The directory or file could not be created, the pragmas failed, or a
    /// migration failed. The message names the path for the first two.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(directory) = path.parent()
            && !directory.as_os_str().is_empty()
        {
            std::fs::create_dir_all(directory).map_err(|source| StoreError::Directory {
                path: directory.to_path_buf(),
                source,
            })?;
        }
        let connection = Connection::open(&path)?;
        Self::prepare(connection, Some(path))
    }

    /// A store in this process's memory, for tests.
    ///
    /// # Errors
    ///
    /// The schema did not apply.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::prepare(Connection::open_in_memory()?, None)
    }

    fn prepare(connection: Connection, path: Option<PathBuf>) -> Result<Self, StoreError> {
        // WAL for concurrent readers while a turn writes; `foreign_keys` per
        // connection, which is the only place SQLite will take it.
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let mut connection = connection;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
        })
    }

    /// The file this store reads, when it is a file.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The schema version this database is at.
    ///
    /// # Errors
    ///
    /// The pragma could not be read.
    pub fn version(&self) -> Result<u32, StoreError> {
        let connection = self.lock();
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        Ok(u32::try_from(version).unwrap_or_default())
    }

    /// Note that a workspace exists, so a session can point at it.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn remember_workspace(&self, id: &str, root: &str) -> Result<(), StoreError> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO workspaces (id, root, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET root = excluded.root",
            params![id, root, jiff::Timestamp::now().to_string()],
        )?;
        Ok(())
    }

    /// Record a host so it is still known after a restart.
    ///
    /// The transport's address rather than the last thing that answered: what a
    /// record is for is what to try next time, and a cached success would make a
    /// host that went away look present.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn save_host(&self, host: &HostRecord) -> Result<(), StoreError> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO hosts (id, kind, address, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, address = excluded.address",
            params![
                host.id,
                kind_text(host.kind),
                host.address,
                jiff::Timestamp::now().to_string(),
            ],
        )?;
        Ok(())
    }

    /// Every host the daemon has been told about, in the order it was told.
    ///
    /// Ordered by `rowid` rather than `created_at`, because that column is a
    /// human-readable `jiff` timestamp and its fraction is written at whatever
    /// width it needs — none, 3, 6 or 9 digits. Comparing those strings is not
    /// comparing times: `…T00:00:00Z` sorts *after* `…T00:00:00.000001Z`, and
    /// `…T00:00:00.1Z` after `…T00:00:00.123456789Z`, both because `Z` and `1`
    /// outrank the digit that continues the shorter fraction. Two hosts saved in
    /// the same second could therefore come back in the wrong order, and did: the
    /// macOS run of this crate's suite listed a host added second first. `rowid`
    /// is insertion order by construction and an upsert keeps it, which is the
    /// property a list of hosts actually wants.
    ///
    /// # Errors
    ///
    /// A row could not be read.
    pub fn hosts(&self) -> Result<Vec<HostRecord>, StoreError> {
        let connection = self.lock();
        let mut statement =
            connection.prepare("SELECT id, kind, address FROM hosts ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok(HostRecord {
                id: row.get(0)?,
                kind: kind_from(&row.get::<_, String>(1)?),
                address: row.get(2)?,
            })
        })?;
        let mut hosts = Vec::new();
        for row in rows {
            hosts.push(row?);
        }
        Ok(hosts)
    }

    /// Record that a person approved this agent's binary.
    ///
    /// A pin is replaced rather than accumulated: the row is the binary currently
    /// trusted for an agent, and two rows would be two answers to one question.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn save_agent_pin(&self, agent_id: &str, fingerprint: &str) -> Result<(), StoreError> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO agent_pins (agent_id, fingerprint, trusted_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (agent_id) DO UPDATE SET fingerprint = excluded.fingerprint,
             trusted_at = excluded.trusted_at",
            params![agent_id, fingerprint, jiff::Timestamp::now().to_string()],
        )?;
        Ok(())
    }

    /// Every pin a person has recorded, oldest first.
    ///
    /// # Errors
    ///
    /// A row could not be read.
    pub fn agent_pins(&self) -> Result<Vec<AgentPin>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT agent_id, fingerprint FROM agent_pins ORDER BY trusted_at, agent_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(AgentPin {
                agent_id: row.get(0)?,
                fingerprint: row.get(1)?,
            })
        })?;
        let mut pins = Vec::new();
        for row in rows {
            pins.push(row?);
        }
        Ok(pins)
    }

    /// Write a session's current shape, creating the row or replacing it.
    ///
    /// The whole summary is written rather than a field at a time: the daemon's
    /// record is the authority, and a partial update would have to know which
    /// fields changed to avoid resurrecting a stale one.
    ///
    /// # Errors
    ///
    /// The row could not be written, or the session names a workspace that was
    /// never remembered.
    pub fn save_session(&self, summary: &SessionSummary) -> Result<(), StoreError> {
        let connection = self.lock();
        // A session's workspace is the daemon's, and a daemon with no profile has
        // an empty id: remembering it here keeps the foreign key honest without a
        // second call every caller could forget.
        connection.execute(
            "INSERT OR IGNORE INTO workspaces (id, root, created_at) VALUES (?1, '', ?2)",
            params![summary.workspace_id, jiff::Timestamp::now().to_string()],
        )?;
        connection.execute(
            "INSERT INTO sessions (id, workspace_id, agent_id, title, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET
                 title = excluded.title,
                 status = excluded.status,
                 updated_at = excluded.updated_at",
            params![
                summary.id,
                summary.workspace_id,
                summary.agent_id,
                summary.title,
                status_text(&summary.status),
                summary.created_at,
                summary.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Move a session's status without rewriting the rest of it.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn set_status(
        &self,
        id: &str,
        status: &SessionStatus,
        updated_at: &str,
    ) -> Result<(), StoreError> {
        let connection = self.lock();
        connection.execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status_text(status), updated_at],
        )?;
        Ok(())
    }

    /// Mark every session that is not closed as closed.
    ///
    /// Called once at startup. A session that was `busy` when the process died is
    /// not busy in the new one — nothing is running it — and a client that lists it
    /// as busy would wait for a turn that will never finish. `closed` is the honest
    /// status for "this process did not start it".
    ///
    /// # Errors
    ///
    /// The update failed.
    pub fn close_open_sessions(&self) -> Result<usize, StoreError> {
        let connection = self.lock();
        let closed = connection.execute(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE status != ?1",
            params![
                status_text(&SessionStatus::Closed),
                jiff::Timestamp::now().to_string()
            ],
        )?;
        Ok(closed)
    }

    /// One stored session, when the profile has ever had it.
    ///
    /// # Errors
    ///
    /// The row could not be read.
    pub fn session(&self, id: &str) -> Result<Option<SessionSummary>, StoreError> {
        let connection = self.lock();
        let row = connection
            .query_row(
                "SELECT id, workspace_id, agent_id, title, status, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(summary_from))
    }

    /// Every stored session, oldest first.
    ///
    /// # Errors
    ///
    /// A row could not be read.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT id, workspace_id, agent_id, title, status, created_at, updated_at
             FROM sessions ORDER BY created_at, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(summary_from(row?));
        }
        Ok(summaries)
    }

    /// The ids this profile has used, so a restarted daemon does not reuse one.
    ///
    /// # Errors
    ///
    /// The ids could not be read.
    pub fn session_ids(&self) -> Result<Vec<String>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT id FROM sessions")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Append one event, replacing the row when the sequence is already stored.
    ///
    /// A sequence is the session's own, so "already there" means the same event
    /// arrived twice — a reconnect's replay — and replacing it keeps the table in
    /// step with the ring instead of failing a turn over a duplicate.
    ///
    /// # Errors
    ///
    /// The row could not be written, or the session is not stored yet.
    pub fn append_event(&self, event: &SessionEvent) -> Result<(), StoreError> {
        let connection = self.lock();
        let at = event
            .at
            .parse::<jiff::Timestamp>()
            .map(|stamp| stamp.as_millisecond())
            .unwrap_or_default();
        connection.execute(
            "INSERT INTO events (session_id, seq, at, at_unix_ms, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (session_id, seq) DO UPDATE SET
                 at = excluded.at,
                 at_unix_ms = excluded.at_unix_ms,
                 kind = excluded.kind,
                 payload = excluded.payload",
            params![
                event.session_id,
                i64::try_from(event.seq).unwrap_or(i64::MAX),
                event.at,
                at,
                serde_json::to_string(&event.kind).unwrap_or_default(),
                serde_json::to_string(event)?,
            ],
        )?;
        Ok(())
    }

    /// Everything after `after`, oldest first.
    ///
    /// # Errors
    ///
    /// A row could not be read or parsed as an event.
    pub fn events_after(&self, session: &str, after: u64) -> Result<Vec<SessionEvent>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT payload FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = statement
            .query_map(params![session, i64::try_from(after).unwrap_or(0)], |row| {
                row.get::<_, String>(0)
            })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    /// The newest sequence stored for a session, `0` when it has none.
    ///
    /// # Errors
    ///
    /// The row could not be read.
    pub fn last_seq(&self, session: &str) -> Result<u64, StoreError> {
        let connection = self.lock();
        let seq: Option<i64> = connection
            .query_row(
                "SELECT MAX(seq) FROM events WHERE session_id = ?1",
                params![session],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(seq.and_then(|seq| u64::try_from(seq).ok()).unwrap_or(0))
    }

    /// Write one permission decision.
    ///
    /// # Errors
    ///
    /// The row could not be written.
    pub fn record_grant(&self, grant: &Grant) -> Result<(), StoreError> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO grants (session_id, seq, at, at_unix_ms, tool, decision, decided_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (session_id, seq) DO NOTHING",
            params![
                grant.session_id,
                i64::try_from(grant.seq).unwrap_or(i64::MAX),
                grant.at,
                grant
                    .at
                    .parse::<jiff::Timestamp>()
                    .map(|stamp| stamp.as_millisecond())
                    .unwrap_or_default(),
                grant.tool,
                grant.decision,
                grant.decided_by,
            ],
        )?;
        Ok(())
    }

    /// A session's decisions, oldest first.
    ///
    /// # Errors
    ///
    /// A row could not be read.
    pub fn grants(&self, session: &str) -> Result<Vec<Grant>, StoreError> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT session_id, seq, at, tool, decision, decided_by
             FROM grants WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![session], |row| {
            Ok(Grant {
                session_id: row.get(0)?,
                seq: u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                at: row.get(2)?,
                tool: row.get(3)?,
                decision: row.get(4)?,
                decided_by: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Delete events older than `before`, returning how many went.
    ///
    /// Grant rows are kept: a permission decision is an audit record, and the
    /// audit is the one thing a retention window must not quietly erase. Events
    /// are the volume — a turn is thousands of them — and the transcript beside the
    /// database still holds them.
    ///
    /// # Errors
    ///
    /// The delete failed.
    pub fn compact(&self, before: jiff::Timestamp) -> Result<usize, StoreError> {
        let connection = self.lock();
        let deleted = connection.execute(
            "DELETE FROM events WHERE at_unix_ms < ?1",
            params![before.as_millisecond()],
        )?;
        Ok(deleted)
    }

    /// When this store was last swept, by reading the oldest event it still has.
    ///
    /// Not part of retention: it is what a `doctor`-style report needs to say
    /// "compact is running" without a second table.
    ///
    /// # Errors
    ///
    /// The row could not be read.
    pub fn oldest_event(&self) -> Result<Option<jiff::Timestamp>, StoreError> {
        let connection = self.lock();
        let stamp: Option<i64> = connection
            .query_row("SELECT MIN(at_unix_ms) FROM events", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(stamp.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the connection poisons the guard, and the database
        // file is not in an unknown state: SQLite rolls back the statement that
        // panicked. Recovering is what keeps one bad turn from taking the store.
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Bring `connection` up to [`SCHEMA_VERSION`], or say why it cannot be.
///
/// One transaction per step, with `user_version` bumped inside it: a crash between
/// the `CREATE TABLE`s and the version bump would otherwise leave a database that
/// claims to be older than it is and fails on the second step's first statement.
fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let found: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let found = u32::try_from(found).unwrap_or(u32::MAX);
    if found > SCHEMA_VERSION {
        return Err(StoreError::Newer {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS {
        if *version <= found {
            continue;
        }
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", i64::from(*version))?;
        transaction.commit()?;
    }
    Ok(())
}

/// A host the daemon has been told about, as the database keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRecord {
    /// Identifier the API uses: `local`, `ssh:user@machine`, `wsl:Ubuntu`.
    pub id: String,
    /// How the daemon reaches it.
    pub kind: HostKind,
    /// What the transport needs to name it; absent for the local host.
    pub address: Option<String>,
}

/// An agent binary a person approved, as the database keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPin {
    /// The agent the approval belongs to.
    pub agent_id: String,
    /// SHA-256 of the program's bytes when it was approved.
    pub fingerprint: String,
}

/// One permission decision, as the database keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The session the decision belonged to.
    pub session_id: String,
    /// The broker's audit sequence, which is what makes the write idempotent.
    pub seq: u64,
    /// When it was decided.
    pub at: String,
    /// What was asked about: a command line, a path, a tool.
    pub tool: String,
    /// The words a person reads: `allow-workspace`, `deny`, `ask`.
    pub decision: String,
    /// Who decided: `policy:<rule>` or `client:<session>`.
    pub decided_by: String,
}

fn summary_from(
    (id, workspace_id, agent_id, title, status, created_at, updated_at): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ),
) -> SessionSummary {
    SessionSummary {
        id,
        workspace_id,
        agent_id,
        title,
        status: status_from(&status),
        created_at,
        updated_at,
    }
}

/// The transport as the `hosts.kind` column writes it: the wire spelling, so a
/// row read by a person and a body read by a client say the same word.
fn kind_text(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Local => "local",
        HostKind::Ssh => "ssh",
        HostKind::Wsl => "wsl",
    }
}

/// The kind a stored row names. The column's `CHECK` already refuses anything
/// else; a row from a schema this build has not met reads as the local host rather
/// than reviving a transport with no implementation behind it.
fn kind_from(text: &str) -> HostKind {
    match text {
        "ssh" => HostKind::Ssh,
        "wsl" => HostKind::Wsl,
        _ => HostKind::Local,
    }
}

/// The status as it is written: the wire spelling, so a database read by a person
/// and a body read by a client say the same word.
fn status_text(status: &SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "idle".to_owned())
}

/// The inverse of [`status_text`]. An unknown word reads as `closed`: a daemon
/// that stopped writing a variant should not resurrect a conversation as busy.
fn status_from(text: &str) -> SessionStatus {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .unwrap_or(SessionStatus::Closed)
}

/// Why the store could not do what it was asked.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The directory holding the database could not be created.
    #[error("cannot create {}: {source}", path.display())]
    Directory {
        /// The directory that failed.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },

    /// SQLite refused the statement.
    #[error("the store: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was written by a newer daemon.
    #[error(
        "this database is at schema {found}, and this build knows {supported}; upgrade the \
         daemon rather than opening it with an older one"
    )]
    Newer {
        /// The version the file says.
        found: u32,
        /// The version this build understands.
        supported: u32,
    },

    /// A stored event or payload is not the JSON this build expects.
    #[error("a stored row is not readable: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            workspace_id: "demo".into(),
            agent_id: "mock".into(),
            title: "a conversation".into(),
            status: SessionStatus::Idle,
            created_at: "2026-11-03T10:00:00Z".into(),
            updated_at: "2026-11-03T10:00:00Z".into(),
        }
    }

    fn event(session: &str, seq: u64, at: &str) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: session.to_owned(),
            kind: acpilot_proto::event::SessionEventKind::MessageChunk,
            at: at.to_owned(),
            payload: serde_json::json!({ "text": format!("line {seq}") }),
        }
    }

    #[test]
    fn opening_a_store_twice_is_the_same_database() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("nested").join(FILE_NAME);
        {
            let store = Store::open(&path).expect("the first open creates the file");
            store
                .remember_workspace("demo", "/tmp/demo")
                .expect("workspace");
            store.save_session(&summary("s1")).expect("session");
            assert_eq!(store.version().expect("version"), SCHEMA_VERSION);
        }
        let again = Store::open(&path).expect("the second open runs no migration");
        assert_eq!(again.version().expect("version"), SCHEMA_VERSION);
        assert_eq!(
            again.session("s1").expect("read").expect("there").title,
            "a conversation"
        );
    }

    #[test]
    fn a_file_store_journals_in_wal_and_enforces_foreign_keys() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = Store::open(directory.path().join(FILE_NAME)).expect("open");
        {
            let connection = store.lock();
            let mode: String = connection
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .expect("journal mode");
            assert_eq!(mode, "wal");
            let keys: i64 = connection
                .pragma_query_value(None, "foreign_keys", |row| row.get(0))
                .expect("foreign keys");
            assert_eq!(keys, 1);
        }
        // An event for a session that was never stored is refused, not orphaned.
        let error = store
            .append_event(&event("ghost", 1, "2026-11-03T10:00:00Z"))
            .expect_err("a foreign key that is on refuses this");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error}");
    }

    #[test]
    fn events_come_back_after_the_sequence_asked_for() {
        let store = Store::in_memory().expect("open");
        store.save_session(&summary("s1")).expect("session");
        for seq in 1..=5 {
            store
                .append_event(&event("s1", seq, "2026-11-03T10:00:00Z"))
                .expect("event");
        }
        let events = store.events_after("s1", 2).expect("read");
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(store.last_seq("s1").expect("last"), 5);
    }

    #[test]
    fn re_delivering_an_event_updates_it_instead_of_failing() {
        let store = Store::in_memory().expect("open");
        store.save_session(&summary("s1")).expect("session");
        store
            .append_event(&event("s1", 1, "2026-11-03T10:00:00Z"))
            .expect("first");
        store
            .append_event(&event("s1", 1, "2026-11-03T10:00:01Z"))
            .expect("the replay");
        let events = store.events_after("s1", 0).expect("read");
        assert_eq!(events.len(), 1, "one sequence is one row");
        assert_eq!(events[0].at, "2026-11-03T10:00:01Z");
    }

    #[test]
    fn retention_drops_old_events_and_keeps_the_decisions() {
        let store = Store::in_memory().expect("open");
        store.save_session(&summary("s1")).expect("session");
        store
            .append_event(&event("s1", 1, "2026-09-01T10:00:00Z"))
            .expect("an old event");
        store
            .append_event(&event("s1", 2, "2026-11-03T10:00:00Z"))
            .expect("a new event");
        store
            .record_grant(&Grant {
                session_id: "s1".into(),
                seq: 1,
                at: "2026-09-01T10:00:00Z".into(),
                tool: "shell".into(),
                decision: "allow-workspace".into(),
                decided_by: "policy:allow-workspace".into(),
            })
            .expect("an old decision");

        let cutoff = "2026-10-04T00:00:00Z"
            .parse::<jiff::Timestamp>()
            .expect("cutoff");
        assert_eq!(store.compact(cutoff).expect("compact"), 1);
        assert_eq!(
            store
                .events_after("s1", 0)
                .expect("read")
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(
            store.grants("s1").expect("decisions").len(),
            1,
            "the audit outlives the window"
        );
    }

    #[test]
    fn a_status_moves_without_rewriting_the_conversation() {
        let store = Store::in_memory().expect("open");
        store.save_session(&summary("s1")).expect("session");
        store
            .set_status("s1", &SessionStatus::Busy, "2026-11-03T11:00:00Z")
            .expect("status");
        let stored = store.session("s1").expect("read").expect("there");
        assert_eq!(stored.status, SessionStatus::Busy);
        assert_eq!(stored.updated_at, "2026-11-03T11:00:00Z");
        assert_eq!(stored.title, "a conversation");
    }

    #[test]
    fn a_host_is_stored_and_read_back() {
        let store = Store::in_memory().expect("open");
        store
            .save_host(&HostRecord {
                id: "local".into(),
                kind: HostKind::Local,
                address: None,
            })
            .expect("local");
        store
            .save_host(&HostRecord {
                id: "ssh:me@box".into(),
                kind: HostKind::Ssh,
                address: Some("me@box".into()),
            })
            .expect("ssh");
        let hosts = store.hosts().expect("read");
        assert_eq!(hosts.len(), 2);
        // By id rather than by position: this test is about what a row reads back
        // as, and a position would make it fail for a reason it does not name.
        let ssh = hosts
            .iter()
            .find(|host| host.id == "ssh:me@box")
            .expect("the ssh host");
        assert_eq!(ssh.kind, HostKind::Ssh);
        assert_eq!(ssh.address.as_deref(), Some("me@box"));
    }

    #[test]
    fn hosts_come_back_in_the_order_they_were_added() {
        let store = Store::in_memory().expect("open");
        store
            .save_host(&HostRecord {
                id: "local".into(),
                kind: HostKind::Local,
                address: None,
            })
            .expect("local");
        store
            .save_host(&HostRecord {
                id: "ssh:me@box".into(),
                kind: HostKind::Ssh,
                address: Some("me@box".into()),
            })
            .expect("ssh");

        // The two spellings a macOS run produced from ordinary saves: one whole
        // second, one microsecond later. `2026-11-05T00:00:00.000001Z` compares
        // *before* `2026-11-05T00:00:00Z` as text, which is why the list cannot be
        // ordered by that column without disagreeing with the clock.
        store
            .lock()
            .execute(
                "UPDATE hosts SET created_at = '2026-11-05T00:00:00Z' WHERE id = 'local'",
                [],
            )
            .expect("local timestamp");
        store
            .lock()
            .execute(
                "UPDATE hosts SET created_at = '2026-11-05T00:00:00.000001Z' \
                 WHERE id = 'ssh:me@box'",
                [],
            )
            .expect("ssh timestamp");

        let hosts = store.hosts().expect("read");
        assert_eq!(
            hosts
                .iter()
                .map(|host| host.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "ssh:me@box"],
            "oldest first is the order the daemon was told, not the text of a column"
        );
    }

    #[test]
    fn a_pin_is_replaced_rather_than_accumulated() {
        let store = Store::in_memory().expect("open");
        store.save_agent_pin("mock", "aaa").expect("first");
        store.save_agent_pin("mock", "bbb").expect("second");
        let pins = store.agent_pins().expect("read");
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].agent_id, "mock");
        assert_eq!(pins[0].fingerprint, "bbb");
    }

    #[test]
    fn a_grant_keeps_the_rule_that_decided_it() {
        let store = Store::in_memory().expect("open");
        store.save_session(&summary("s1")).expect("session");
        store
            .record_grant(&Grant {
                session_id: "s1".into(),
                seq: 7,
                at: "2026-11-03T10:00:00Z".into(),
                tool: "cargo test".into(),
                decision: "allow-workspace".into(),
                decided_by: "policy:allow-workspace".into(),
            })
            .expect("grant");
        let grants = store.grants("s1").expect("read");
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].decided_by, "policy:allow-workspace");
    }

    #[test]
    fn an_unknown_status_reads_as_closed_rather_than_resurrecting_a_session() {
        assert_eq!(status_from("busy"), SessionStatus::Busy);
        assert_eq!(status_from("retired-in-2031"), SessionStatus::Closed);
    }
}
