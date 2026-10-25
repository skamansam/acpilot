//! The append-only transcript: one `session.jsonl` per conversation.
//!
//! A live session keeps its events in a ring, and a ring forgets. A transcript is
//! what survives a restart, a closed tab and a client that attaches an hour later:
//! every event, every permission decision, and one `meta` line saying whose
//! conversation this is. It is the file the service phase's task P3-7 compacts into
//! SQLite, so the format is JSON Lines rather than anything clever — one record per
//! line, readable with `jq`, appendable without rewriting what is already there.
//!
//! # Append-only means append-only
//!
//! [`Transcript::create`] opens with `create_new`, so an existing transcript is
//! refused rather than truncated. Two conversations must not share a file: if a
//! daemon restarted and handed the same session id to a new conversation, an
//! append would mix two transcripts into one that neither reader can untangle.
//! The caller's job is a path nothing has used; this module makes the mistake loud.
//!
//! A line is written whole, in one `write_all` on a file opened `O_APPEND`. That
//! is what lets the reader say something useful about a crash: a partial record can
//! only ever be the *last* one, so [`read`] stops there instead of refusing the
//! transcript. Nothing is `fsync`ed per line — a turn is thousands of records, and
//! a power cut losing the tail is the same trade every editor's journal makes.
//! A process crash loses nothing: the bytes are with the kernel already.
//!
//! # What a record is
//!
//! The tag is `record`, not `kind`, because [`Entry::Event`] already carries a
//! `kind` of its own and two fields called `kind` on one line is a bug waiting for
//! a reader to pick the wrong one. `meta` is written once and first; `event` is the
//! same [`SessionEvent`] the stream carried, sequence number and all, so a reader
//! can replay from any `after` a client names; `grant` is a permission decision —
//! the row the audit page shows, denormalised into strings and options that do not
//! move when the broker's internal types do.
//!
//! Grants are written by the session that made the requests, in audit order, not by
//! the broker: the broker answers one question at a time and knows nothing about
//! files, while the session knows which transcript is its own.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use acpilot_proto::event::{EventSeq, SessionEvent};
use acpilot_proto::permission::PermissionOption;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::permission::AuditEntry;

/// The file a transcript is written to, inside whatever directory a caller gave
/// the conversation.
///
/// A directory per session rather than a flat `<id>.jsonl` because a transcript
/// is a file that will grow siblings: the P3-7 compaction writes a database beside
/// the jsonl it read, and the database belongs to the same session.
pub const FILE_NAME: &str = "session.jsonl";

/// The format version on the `meta` line.
///
/// Bumped when a reader could not understand an older file; adding a record kind
/// does not bump it, because a reader that ignores what it does not know still
/// replays the events correctly.
pub const VERSION: u32 = 1;

/// What went wrong writing or reading a transcript.
#[derive(Debug, Error)]
pub enum TranscriptError {
    /// Something is already at that path, and a transcript is never rewritten.
    #[error("{} already holds a transcript", path.display())]
    Exists {
        /// The path that was asked for.
        path: PathBuf,
    },

    /// The file could not be created or appended to.
    #[error("cannot write {}: {source}", path.display())]
    Write {
        /// The transcript being written.
        path: PathBuf,
        /// Why the write failed.
        #[source]
        source: io::Error,
    },

    /// The file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The transcript being read.
        path: PathBuf,
        /// Why the read failed.
        #[source]
        source: io::Error,
    },

    /// A record could not be turned into a line.
    #[error("cannot encode a transcript record: {source}")]
    Encode {
        /// The serialization failure, unchanged.
        #[source]
        source: serde_json::Error,
    },

    /// A line in the middle of the file is not a record.
    ///
    /// Only a line that is *not* the tail is this error: a partial tail is a crash
    /// during a write, and [`read`] stops there instead.
    #[error("{} line {line} is not a transcript record: {source}", path.display())]
    Record {
        /// The transcript being read.
        path: PathBuf,
        /// 1-based line number, so an error names the line a person must look at.
        line: usize,
        /// What the parser said.
        #[source]
        source: serde_json::Error,
    },
}

/// One line of a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Entry {
    /// Who this conversation is, written once, first.
    Meta(Meta),
    /// One event, exactly as the stream carried it.
    Event(SessionEvent),
    /// One permission decision.
    Grant(Grant),
}

/// The first line: which conversation this file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Format version, so a future reader can refuse what it cannot parse.
    pub version: u32,
    /// The daemon's session id.
    pub session: String,
    /// Workspace the session edits.
    pub workspace: String,
    /// Agent the session ran.
    pub agent: String,
    /// RFC 3339 timestamp of the session's creation.
    pub created_at: String,
}

/// One permission decision, flattened for the file.
///
/// The broker's [`AuditEntry`] is the source, but its `decided_by` is an enum whose
/// spelling is a presentation detail and whose paths are `PathBuf`s that need not
/// be UTF-8. A transcript is a record of what was decided, so it keeps the words:
/// `user`, `policy:<id>`, and a path with unrepresentable bytes made lossy rather
/// than failing the write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The broker's audit sequence number, monotonic across the profile.
    pub seq: u64,
    /// RFC 3339 timestamp of the decision.
    pub at: String,
    /// Session that asked.
    pub session: String,
    /// Agent that asked, when the request carried one.
    pub agent: Option<String>,
    /// What kind of request it was: `file_write`, `terminal`, …
    pub kind: String,
    /// Tool label, when the request had one.
    pub tool: Option<String>,
    /// Canonical path, when the request concerned one.
    pub path: Option<String>,
    /// The request as it was shown to the person.
    pub title: String,
    /// What was decided.
    pub option: PermissionOption,
    /// Who or what decided it: `user`, `policy:<rule-id>`, `timeout`, …
    pub decided_by: String,
}

impl From<AuditEntry> for Grant {
    fn from(entry: AuditEntry) -> Self {
        Self {
            seq: entry.seq,
            at: entry.at,
            session: entry.session,
            agent: entry.agent,
            // The wire spelling (`file_write`), not the Rust variant name: the
            // same word the API and the agent's own permission request use.
            kind: serde_json::to_value(entry.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "other".to_owned()),
            tool: entry.tool,
            path: entry.path.map(|path| path.to_string_lossy().into_owned()),
            title: entry.title,
            option: entry.option,
            // `Display`, not `as_str`: the audit line spells "policy:allow-workspace",
            // and the rule id is the half that is worth keeping.
            decided_by: entry.decided_by.to_string(),
        }
    }
}

/// A transcript being appended to.
///
/// One per session, opened when the session starts and dropped when it ends. The
/// file is held open and every record is one `write_all`, so the cost of a
/// transcript is a syscall per event rather than a reopen.
#[derive(Debug)]
pub struct Transcript {
    path: PathBuf,
    file: Mutex<File>,
}

impl Transcript {
    /// Start the transcript at `path` with its `meta` line.
    ///
    /// Creates the parent directory, because the daemon gives each session its own
    /// directory and a missing one is the daemon's layout, not a caller's mistake.
    ///
    /// # Errors
    ///
    /// [`TranscriptError::Exists`] when something is already there, and
    /// [`TranscriptError::Write`] when the file or its directory cannot be made.
    /// The `meta` line is written here rather than by the caller so a transcript
    /// can never exist without one.
    pub fn create(path: impl Into<PathBuf>, meta: &Meta) -> Result<Self, TranscriptError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| TranscriptError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    TranscriptError::Exists { path: path.clone() }
                } else {
                    TranscriptError::Write {
                        path: path.clone(),
                        source,
                    }
                }
            })?;
        let transcript = Self {
            path,
            file: Mutex::new(file),
        };
        transcript.append(&Entry::Meta(meta.clone()))?;
        Ok(transcript)
    }

    /// The file this transcript is written to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, as the stream carried it.
    ///
    /// # Errors
    ///
    /// [`TranscriptError::Encode`] or [`TranscriptError::Write`]; a full disk is
    /// the caller's to decide about, and the daemon logs it rather than failing the
    /// turn it was recording.
    pub fn event(&self, event: &SessionEvent) -> Result<(), TranscriptError> {
        self.append(&Entry::Event(event.clone()))
    }

    /// Append one permission decision.
    ///
    /// # Errors
    ///
    /// The same two as [`Self::event`].
    pub fn grant(&self, entry: AuditEntry) -> Result<(), TranscriptError> {
        self.append(&Entry::Grant(entry.into()))
    }

    /// Append one record.
    ///
    /// # Errors
    ///
    /// [`TranscriptError::Encode`] when the record is not serializable (nothing
    /// here is not), and [`TranscriptError::Write`] when the file refuses.
    pub fn append(&self, entry: &Entry) -> Result<(), TranscriptError> {
        let mut line =
            serde_json::to_vec(entry).map_err(|source| TranscriptError::Encode { source })?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        file.write_all(&line)
            .map_err(|source| TranscriptError::Write {
                path: self.path.clone(),
                source,
            })
    }
}

/// Every record in the transcript at `path`, oldest first.
///
/// # Errors
///
/// [`TranscriptError::Read`] when the file cannot be read, and
/// [`TranscriptError::Record`] when a line that is not the last one is not a
/// record. A partial final line is a crash during a write, so it ends the replay
/// rather than failing it.
pub fn read(path: &Path) -> Result<Vec<Entry>, TranscriptError> {
    let text = fs::read_to_string(path).map_err(|source| TranscriptError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let lines: Vec<&str> = text.split('\n').collect();
    // `split` leaves an empty string after a trailing newline, so the last
    // *element* is only a fragment when the file does not end with one.
    let partial_tail = !text.ends_with('\n');
    let mut entries = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            Err(_) if partial_tail && index + 1 == lines.len() => break,
            Err(source) => {
                return Err(TranscriptError::Record {
                    path: path.to_path_buf(),
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(entries)
}

/// The events in the transcript at `path` after `after`, oldest first.
///
/// The same contract as `GET /v1/sessions/{id}/events?after=`: a client that saw
/// sequence `n` asks for everything after it. Records of other kinds are skipped,
/// not refused, so a file written by a newer daemon still replays.
///
/// # Errors
///
/// The same as [`read`].
pub fn replay(path: &Path, after: EventSeq) -> Result<Vec<SessionEvent>, TranscriptError> {
    Ok(read(path)?
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Event(event) if event.seq > after => Some(event),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::DecidedBy;
    use acpilot_proto::event::SessionEventKind;
    use acpilot_proto::permission::PermissionKind;

    fn meta() -> Meta {
        Meta {
            version: VERSION,
            session: "s1".to_owned(),
            workspace: "work".to_owned(),
            agent: "mock".to_owned(),
            created_at: "2026-10-25T00:00:00Z".to_owned(),
        }
    }

    fn event(seq: EventSeq) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: "s1".to_owned(),
            kind: SessionEventKind::MessageChunk,
            at: format!("2026-10-25T00:00:0{seq}Z"),
            payload: serde_json::json!({ "text": format!("chunk {seq}") }),
        }
    }

    fn audit(seq: u64) -> AuditEntry {
        AuditEntry {
            seq,
            at: "2026-10-25T00:00:05Z".to_owned(),
            session: "s1".to_owned(),
            agent: Some("mock".to_owned()),
            kind: PermissionKind::FileWrite,
            tool: Some("edit".to_owned()),
            path: Some(PathBuf::from("/work/src/main.rs")),
            title: "Write src/main.rs".to_owned(),
            option: PermissionOption::AllowOnce,
            decided_by: DecidedBy::Policy("allow-workspace".to_owned()),
        }
    }

    #[test]
    fn a_new_transcript_starts_with_its_meta_and_is_never_rewritten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("s1").join(FILE_NAME);
        let transcript = Transcript::create(&path, &meta()).expect("the transcript starts");
        assert_eq!(transcript.path(), path);

        let entries = read(&path).expect("it reads back");
        assert_eq!(entries, vec![Entry::Meta(meta())]);

        let again = Transcript::create(&path, &meta());
        assert!(
            matches!(again, Err(TranscriptError::Exists { .. })),
            "a second conversation must not be appended to the first: {again:?}"
        );
    }

    #[test]
    fn events_replay_from_the_sequence_a_client_last_saw() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE_NAME);
        let transcript = Transcript::create(&path, &meta()).expect("the transcript starts");
        for seq in 1..=3 {
            transcript.event(&event(seq)).expect("the event is written");
        }

        let replayed = replay(&path, 1).expect("the transcript replays");
        assert_eq!(replayed, vec![event(2), event(3)]);
        assert_eq!(
            replay(&path, 3).expect("the transcript replays"),
            Vec::new(),
            "a client that saw the last event is owed nothing"
        );
    }

    #[test]
    fn a_grant_keeps_the_words_that_decided_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE_NAME);
        let transcript = Transcript::create(&path, &meta()).expect("the transcript starts");
        transcript.grant(audit(7)).expect("the grant is written");

        let entries = read(&path).expect("it reads back");
        let Some(Entry::Grant(grant)) = entries.last() else {
            panic!("the grant is the last record: {entries:?}");
        };
        assert_eq!(grant.seq, 7);
        assert_eq!(grant.kind, "file_write");
        assert_eq!(grant.decided_by, "policy:allow-workspace");
        assert_eq!(grant.path.as_deref(), Some("/work/src/main.rs"));
        assert_eq!(grant.option, PermissionOption::AllowOnce);
    }

    #[test]
    fn a_crash_mid_write_costs_the_last_record_and_nothing_before_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE_NAME);
        let transcript = Transcript::create(&path, &meta()).expect("the transcript starts");
        transcript.event(&event(1)).expect("the event is written");

        // What a crash between `write` and its newline leaves: half a record.
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("the file opens");
        file.write_all(br#"{"record":"event","seq":2"#)
            .expect("the fragment is written");

        let entries = read(&path).expect("a torn tail is not a broken transcript");
        assert_eq!(
            entries,
            vec![Entry::Meta(meta()), Entry::Event(event(1))],
            "the records before the tear survive"
        );
    }

    #[test]
    fn a_broken_line_in_the_middle_names_its_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE_NAME);
        let transcript = Transcript::create(&path, &meta()).expect("the transcript starts");
        transcript.event(&event(1)).expect("the event is written");
        let mut text = fs::read_to_string(&path).expect("it reads");
        text.insert_str(0, "not a record\n");
        fs::write(&path, text).expect("the file is rewritten by the test");

        let error = read(&path).expect_err("a corrupt middle line is an error");
        assert!(
            matches!(error, TranscriptError::Record { line: 1, .. }),
            "{error}"
        );
    }
}
