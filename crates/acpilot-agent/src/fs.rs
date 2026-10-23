//! The client half of ACP's filesystem methods, and the guard in front of it.
//!
//! An ACP agent does not open files. It asks its *client* to, because the client
//! is the side that knows where the workspace is and which machine it is on. So
//! `fs/read_text_file` and `fs/write_text_file` arrive here, and every byte they
//! move goes through [`Host`] rather than `std::fs` — the same code path serves a
//! laptop and, once P3-9 lands the SFTP channel, a remote workspace.
//!
//! # The workspace is the boundary, and it is checked after resolution
//!
//! A path is resolved *on the host* (`Host::canonicalize`, which follows
//! symlinks) and only then compared with the workspace root. Comparing the text
//! the agent sent would miss `workspace/link`, which is a path that looks
//! contained and points at `/etc`.
//!
//! A relative path is refused rather than joined onto the workspace: the protocol
//! says absolute, and quietly picking a base directory turns a bug in an agent
//! into a file in an unexpected place.
//!
//! Reads only ever happen inside the workspace. Reads outside it are refused with
//! a typed error; there is no approval flow for them, and inventing one before
//! P2-6 owns the policy engine would mean two answers to the same question.
//!
//! # The shadow write
//!
//! A write inside the workspace goes straight to disk. A write outside it does
//! not touch anything: a [`PendingWrite`] is published on [`Grants`], the call
//! waits for an approval or a denial, and the file is written only after the
//! approval. That is what makes it a *shadow* write — while the request is
//! pending, the old contents are still the contents, so a UI can show the diff
//! against a file that has not moved, and a crash leaves the disk as it was.
//!
//! Nothing is written speculatively and rolled back. Rolling back is a second
//! chance to write the wrong thing, and a file restored by a rollback is not the
//! same inode the editor had open.
//!
//! # Refusal is the default
//!
//! If nothing is listening for pending writes, the request is refused immediately
//! rather than waiting for an approval that cannot arrive. If something is
//! listening and never answers, [`GRANT_TIMEOUT`] ends the wait. The agent gets a
//! denial either way: a blocked agent with no path to a decision is a hang, and a
//! hang is what P2-9 found when an unimplemented method went unanswered.
//!
//! # What this module is not
//!
//! Policy is P2-6: rules, path globs, the audit table. This module has exactly one
//! rule — inside the workspace or not — and no opinion about which agents get
//! which exception. Binary detection and the 10 MiB cap belong to the files API
//! (P3-5); a file that is not UTF-8 is refused here rather than converted.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_host::{Host, HostError};
use agent_client_protocol::schema::v1::ErrorCode;
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};

/// How long a write waits for approval before it is refused.
///
/// A person may be reading the diff, so this is minutes rather than seconds. An
/// agent that is blocked on it is meant to be blocked; the timeout exists so it
/// does not stay blocked after the person has gone home.
pub const GRANT_TIMEOUT: Duration = Duration::from_secs(300);

/// How many pending-write announcements a subscriber may fall behind on.
///
/// The same number as [`EVENT_CAPACITY`](crate::session::EVENT_CAPACITY) and for
/// the same reason: a stalled UI should miss announcements, not grow the queue.
pub const GRANT_CAPACITY: usize = 16;

/// What went wrong reading or writing a file for an agent.
#[derive(Debug, Error)]
pub enum FsError {
    /// ACP requires absolute paths; a relative one has no base to resolve from.
    #[error("{} is not an absolute path", path.display())]
    NotAbsolute {
        /// The path the agent sent, unchanged.
        path: PathBuf,
    },

    /// The path resolves outside the workspace.
    #[error("{} is outside the workspace at {}", path.display(), workspace.display())]
    OutsideWorkspace {
        /// The resolved path, which is what the check was made against.
        path: PathBuf,
        /// The workspace root it was compared with.
        workspace: PathBuf,
    },

    /// The file is not valid UTF-8, and this protocol moves text.
    #[error("{} is not valid UTF-8", path.display())]
    NotText {
        /// The file that was read.
        path: PathBuf,
    },

    /// A write the workspace would not allow on its own, with no approval given.
    #[error("{} was not approved for writing", path.display())]
    Denied {
        /// The path that needed a grant.
        path: PathBuf,
    },

    /// A new file's parent directory could not be resolved.
    #[error("{} has no parent directory to resolve", path.display())]
    NoParent {
        /// The path with nothing above it.
        path: PathBuf,
    },

    /// Resolving a path failed on the host.
    #[error("cannot resolve {}: {source}", path.display())]
    Resolve {
        /// The path being resolved.
        path: PathBuf,
        /// Why the host could not resolve it.
        #[source]
        source: HostError,
    },

    /// The host could not read the file.
    #[error("{source}")]
    Read {
        /// The file that was being read.
        path: PathBuf,
        /// Why it failed, which names the path itself.
        #[source]
        source: HostError,
    },

    /// The host could not write the file.
    #[error("{source}")]
    Write {
        /// The file that was being written.
        path: PathBuf,
        /// Why it failed, which names the path itself.
        #[source]
        source: HostError,
    },
}

impl FsError {
    /// The JSON-RPC code an agent should see.
    ///
    /// Refusals are `RequestCancelled` rather than an invented code: ACP has no
    /// "permission denied" of its own, and the closest honest one says the
    /// request was not carried out.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::NotAbsolute { .. } | Self::OutsideWorkspace { .. } | Self::NoParent { .. } => {
                ErrorCode::InvalidParams
            }
            Self::NotText { .. } => ErrorCode::InvalidRequest,
            Self::Denied { .. } => ErrorCode::RequestCancelled,
            Self::Resolve { source, .. }
            | Self::Read { source, .. }
            | Self::Write { source, .. } => host_code(source),
        }
    }

    /// The error as the ACP layer expects it, message included.
    #[must_use]
    pub fn into_protocol(self) -> agent_client_protocol::Error {
        let code = self.code();
        agent_client_protocol::Error::new(i32::from(code), self.to_string())
    }
}

fn host_code(error: &HostError) -> ErrorCode {
    match error {
        HostError::ReadFile { source, .. } | HostError::WriteFile { source, .. } => {
            match source.kind() {
                io::ErrorKind::NotFound => ErrorCode::ResourceNotFound,
                io::ErrorKind::PermissionDenied => ErrorCode::RequestCancelled,
                _ => ErrorCode::InternalError,
            }
        }
        HostError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            ErrorCode::ResourceNotFound
        }
        HostError::Unsupported { .. } => ErrorCode::MethodNotFound,
        _ => ErrorCode::InternalError,
    }
}

/// The filesystem an agent's `fs/*` requests are served from.
///
/// Cloneable and cheap to clone: a session hands one clone to each request
/// handler. The host is shared, so a write the caller makes here and a read the
/// terminal pane makes later see the same machine.
#[derive(Debug, Clone)]
pub struct Fs {
    host: Arc<dyn Host>,
    workspace: PathBuf,
    grants: Arc<Grants>,
}

impl Fs {
    /// Serve `workspace` on `host`, with a fresh [`Grants`].
    ///
    /// The workspace is resolved through the host once, here, so every later
    /// check compares against the real path rather than whatever the caller
    /// spelled.
    pub fn new(host: Arc<dyn Host>, workspace: impl AsRef<Path>) -> Result<Self, FsError> {
        Self::with_grants(host, workspace, Arc::new(Grants::new()))
    }

    /// Serve `workspace` on `host`, sharing an existing grant table.
    ///
    /// A daemon creates one [`Grants`] for the process and passes it in, so every
    /// session's pending writes appear in the same list.
    pub fn with_grants(
        host: Arc<dyn Host>,
        workspace: impl AsRef<Path>,
        grants: Arc<Grants>,
    ) -> Result<Self, FsError> {
        let workspace = workspace.as_ref();
        let resolved = host
            .canonicalize(workspace)
            .map_err(|source| FsError::Resolve {
                path: workspace.to_path_buf(),
                source,
            })?;
        Ok(Self {
            host,
            workspace: resolved,
            grants,
        })
    }

    /// The resolved workspace root.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The pending-write table this client publishes to.
    #[must_use]
    pub fn grants(&self) -> &Arc<Grants> {
        &self.grants
    }

    /// Read a file inside the workspace, optionally one range of lines.
    ///
    /// `line` is 1-based and `limit` counts lines, as ACP defines them. Lines
    /// keep their terminators, so a range of a file reads back as text that can
    /// be written to another file unchanged.
    pub async fn read(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, FsError> {
        let real = self.resolve_existing(path)?;
        if !real.starts_with(&self.workspace) {
            return Err(FsError::OutsideWorkspace {
                path: real,
                workspace: self.workspace.clone(),
            });
        }
        let bytes = self.read_bytes(real.clone()).await?;
        slice(&real, &bytes, line, limit)
    }

    /// Write a file, waiting for approval when it lies outside the workspace.
    ///
    /// `session` is carried on the grant so whoever approves it knows which
    /// conversation asked. It is not used for the decision: policy is P2-6's.
    pub async fn write(&self, session: &str, path: &Path, content: &str) -> Result<(), FsError> {
        let real = self.resolve_for_write(path)?;
        if !real.starts_with(&self.workspace) {
            let approved = self.grants.ask(session, &real, &self.workspace).await;
            if !approved {
                return Err(FsError::Denied { path: real });
            }
        }
        self.write_bytes(real, content).await
    }

    /// Resolve a path that must already exist.
    ///
    /// A failure here is a `Resolve`, not a `Read`: the file was never opened,
    /// and the message names the path the agent gave rather than the one the
    /// host failed to find.
    fn resolve_existing(&self, path: &Path) -> Result<PathBuf, FsError> {
        self.absolute(path)?;
        self.host
            .canonicalize(path)
            .map_err(|source| FsError::Resolve {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Resolve a path that may be a new file, by resolving its parent.
    fn resolve_for_write(&self, path: &Path) -> Result<PathBuf, FsError> {
        self.absolute(path)?;
        if let Ok(real) = self.host.canonicalize(path) {
            return Ok(real);
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(FsError::NoParent {
                path: path.to_path_buf(),
            });
        };
        let parent = self
            .host
            .canonicalize(parent)
            .map_err(|source| FsError::Resolve {
                path: parent.to_path_buf(),
                source,
            })?;
        Ok(parent.join(name))
    }

    fn absolute<'a>(&self, path: &'a Path) -> Result<&'a Path, FsError> {
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(FsError::NotAbsolute {
                path: path.to_path_buf(),
            })
        }
    }

    /// Disk I/O happens on the blocking pool: a host call is a syscall, and a
    /// large file on a slow disk must not stall the session's runtime thread.
    async fn read_bytes(&self, path: PathBuf) -> Result<Vec<u8>, FsError> {
        let host = Arc::clone(&self.host);
        let target = path.clone();
        let result = tokio::task::spawn_blocking(move || host.read_file(&target))
            .await
            .map_err(|error| FsError::Read {
                path: path.clone(),
                source: HostError::Io {
                    operation: "blocking task",
                    source: io::Error::other(error.to_string()),
                },
            })?;
        result.map_err(|source| FsError::Read { path, source })
    }

    async fn write_bytes(&self, path: PathBuf, content: &str) -> Result<(), FsError> {
        let host = Arc::clone(&self.host);
        let target = path.clone();
        let contents = content.as_bytes().to_vec();
        let result = tokio::task::spawn_blocking(move || host.write_file(&target, &contents))
            .await
            .map_err(|error| FsError::Write {
                path: path.clone(),
                source: HostError::Io {
                    operation: "blocking task",
                    source: io::Error::other(error.to_string()),
                },
            })?;
        result.map_err(|source| FsError::Write { path, source })
    }
}

/// The line range an agent asked for, taken from the file's own bytes.
///
/// `split_inclusive` rather than `lines`: a range that stops on the last line
/// keeps its newline if the file had one, so the result can be compared with the
/// file and written back without a diff that is only about a trailing byte.
fn slice(
    path: &Path,
    bytes: &[u8],
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, FsError> {
    let text = std::str::from_utf8(bytes).map_err(|_| FsError::NotText {
        path: path.to_path_buf(),
    })?;
    let start = usize::try_from(line.unwrap_or(1).max(1)).unwrap_or(usize::MAX);
    let take = limit.map_or(usize::MAX, |limit| {
        usize::try_from(limit).unwrap_or(usize::MAX)
    });
    Ok(text
        .split_inclusive('\n')
        .skip(start.saturating_sub(1))
        .take(take)
        .collect())
}

/// The writes waiting for a person to decide.
///
/// One table for the process. A subscriber (the daemon's event stream, later a
/// dialog) receives every pending write; [`Grants::approve`] and
/// [`Grants::deny`] are how it answers. A decision is delivered once: the entry
/// is removed before the answer is sent, so approving the same id twice is a
/// `false`, not a second write.
#[derive(Debug)]
pub struct Grants {
    next: AtomicU64,
    timeout: Option<Duration>,
    table: Mutex<Table>,
    announce: broadcast::Sender<PendingWrite>,
}

#[derive(Debug, Default)]
struct Table {
    pending: HashMap<GrantId, (PendingWrite, oneshot::Sender<bool>)>,
}

impl Default for Grants {
    fn default() -> Self {
        Self::new()
    }
}

impl Grants {
    /// A table that waits [`GRANT_TIMEOUT`] for an answer.
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(Some(GRANT_TIMEOUT))
    }

    /// A table with an explicit wait. `None` waits for as long as the session
    /// lives, which is what a test asserting that a call *blocks* needs.
    #[must_use]
    pub fn with_timeout(timeout: Option<Duration>) -> Self {
        let (announce, _) = broadcast::channel(GRANT_CAPACITY);
        Self {
            next: AtomicU64::new(1),
            timeout,
            table: Mutex::new(Table::default()),
            announce,
        }
    }

    /// Receive every write that needs a decision.
    pub fn subscribe(&self) -> broadcast::Receiver<PendingWrite> {
        self.announce.subscribe()
    }

    /// The writes waiting right now, oldest first.
    ///
    /// A daemon answers `GET /v1/permissions?status=pending` from here: a pending
    /// request is state, not history, and a client that connects after the
    /// announcement still has to see it.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingWrite> {
        let table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        let mut pending: Vec<_> = table
            .pending
            .values()
            .map(|(request, _)| request.clone())
            .collect();
        pending.sort_by_key(|request| request.id);
        pending
    }

    /// Let the write go ahead. `false` if no such request is pending.
    pub fn approve(&self, id: GrantId) -> bool {
        self.decide(id, true)
    }

    /// Refuse the write. `false` if no such request is pending.
    pub fn deny(&self, id: GrantId) -> bool {
        self.decide(id, false)
    }

    fn decide(&self, id: GrantId, allowed: bool) -> bool {
        let entry = {
            let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            table.pending.remove(&id)
        };
        match entry {
            Some((_, decision)) => {
                let _ = decision.send(allowed);
                true
            }
            None => false,
        }
    }

    /// Publish a request and wait for the answer.
    ///
    /// `false` covers every way an approval can fail to happen: nobody listening,
    /// the listener went away, a denial, or the timeout. The caller reports which
    /// one with the path, because from an agent's side they are all "not written".
    async fn ask(&self, session: &str, path: &Path, workspace: &Path) -> bool {
        if self.announce.receiver_count() == 0 {
            tracing::warn!(
                path = %path.display(),
                "a write needs approval and nothing is listening; refusing"
            );
            return false;
        }

        let request = PendingWrite {
            id: self.next_id(),
            session: session.to_owned(),
            path: path.to_path_buf(),
            workspace: workspace.to_path_buf(),
        };
        let (decision, answer) = oneshot::channel();
        {
            let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            table
                .pending
                .insert(request.id, (request.clone(), decision));
        }

        // A send with no receivers is the same "nobody can answer" as above,
        // caught at the moment it matters rather than a moment earlier.
        if self.announce.send(request.clone()).is_err() {
            self.forget(request.id);
            return false;
        }

        let allowed = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, answer).await {
                Ok(Ok(allowed)) => allowed,
                Ok(Err(_)) | Err(_) => false,
            },
            None => matches!(answer.await, Ok(true)),
        };
        // Approve and deny remove the entry themselves; a timeout or a dropped
        // answer leaves it behind, and a stale pending write is a dialog a person
        // can still click.
        self.forget(request.id);
        allowed
    }

    fn next_id(&self) -> GrantId {
        GrantId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn forget(&self, id: GrantId) {
        let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
        table.pending.remove(&id);
    }
}

/// Identifies one pending write, for the answer that approves or denies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantId(u64);

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "grant-{}", self.0)
    }
}

/// A write outside the workspace, waiting for someone to allow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    /// What [`Grants::approve`] and [`Grants::deny`] take.
    pub id: GrantId,
    /// The session that asked, as the daemon names it.
    pub session: String,
    /// The resolved path the write would land on.
    pub path: PathBuf,
    /// The workspace it is outside of.
    pub workspace: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use acpilot_host::LocalHost;
    use tokio::time::timeout;

    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn client(workspace: &Path) -> Fs {
        Fs::new(Arc::new(LocalHost), workspace).expect("the workspace resolves")
    }

    /// Every wait in this module is bounded: a test that blocks forever is a test
    /// that reports nothing.
    const PATIENCE: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_file_in_the_workspace_is_read_from_disk() {
        let dir = workspace();
        fs::write(dir.path().join("notes.txt"), "one\ntwo\n").expect("write");
        let fs = client(dir.path());

        let content = fs
            .read(&dir.path().join("notes.txt"), None, None)
            .await
            .expect("the read happens");
        assert_eq!(content, "one\ntwo\n");
    }

    #[tokio::test]
    async fn a_line_range_is_one_based_and_keeps_its_terminators() {
        let dir = workspace();
        fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\nfour\n").expect("write");
        let fs = client(dir.path());
        let path = dir.path().join("notes.txt");

        let range = fs
            .read(&path, Some(2), Some(2))
            .await
            .expect("the read happens");
        assert_eq!(range, "two\nthree\n");

        // Past the end is an empty answer, not an error: the file exists and the
        // agent asked for a window that does not overlap it.
        let beyond = fs
            .read(&path, Some(9), None)
            .await
            .expect("the read happens");
        assert_eq!(beyond, "");

        // A file with no trailing newline keeps that too.
        fs::write(&path, "only").expect("write");
        assert_eq!(
            fs.read(&path, None, None).await.expect("the read happens"),
            "only"
        );
    }

    #[tokio::test]
    async fn a_read_outside_the_workspace_is_refused() {
        let dir = workspace();
        let elsewhere = workspace();
        let secret = elsewhere.path().join("secret.txt");
        fs::write(&secret, "not for the agent").expect("write");
        let fs = client(dir.path());

        let error = fs.read(&secret, None, None).await.expect_err("refused");
        assert!(
            matches!(&error, FsError::OutsideWorkspace { workspace, .. } if workspace == dir.path()),
            "the refusal names the workspace: {error:?}"
        );
        assert_eq!(error.code(), ErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_workspace_does_not_launder_a_read() {
        let dir = workspace();
        let elsewhere = workspace();
        let secret = elsewhere.path().join("secret.txt");
        fs::write(&secret, "not for the agent").expect("write");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");

        let error = client(dir.path())
            .read(&link, None, None)
            .await
            .expect_err("the link is resolved before it is trusted");
        assert!(
            matches!(error, FsError::OutsideWorkspace { .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_rather_than_guessed_at() {
        let dir = workspace();
        let fs = client(dir.path());

        let error = fs
            .read(Path::new("notes.txt"), None, None)
            .await
            .expect_err("a relative path is not resolved against the workspace");
        assert!(matches!(error, FsError::NotAbsolute { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_binary_file_is_refused_instead_of_converted() {
        let dir = workspace();
        fs::write(dir.path().join("bytes.bin"), [0xff, 0xfe, 0x00]).expect("write");
        let fs = client(dir.path());

        let error = fs
            .read(&dir.path().join("bytes.bin"), None, None)
            .await
            .expect_err("not text");
        assert!(matches!(error, FsError::NotText { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_file_says_which_one_and_why() {
        let dir = workspace();
        let fs = client(dir.path());

        let error = fs
            .read(&dir.path().join("absent.txt"), None, None)
            .await
            .expect_err("no such file");
        assert_eq!(error.code(), ErrorCode::ResourceNotFound);
        assert!(
            error.to_string().contains("absent.txt"),
            "the message names the file: {error}"
        );
    }

    #[tokio::test]
    async fn a_write_inside_the_workspace_lands_on_disk_without_asking() {
        let dir = workspace();
        let fs = client(dir.path());
        let path = dir.path().join("written.txt");

        fs.write("session-1", &path, "hello\n")
            .await
            .expect("an inside write needs no grant");
        assert_eq!(fs::read_to_string(&path).expect("read back"), "hello\n");
        assert!(
            fs.grants().pending().is_empty(),
            "nothing was queued for approval"
        );
    }

    #[tokio::test]
    async fn a_write_outside_the_workspace_waits_and_writes_nothing_first() {
        let dir = workspace();
        let elsewhere = workspace();
        let grants = Arc::new(Grants::with_timeout(None));
        let fs = Arc::new(
            Fs::with_grants(Arc::new(LocalHost), dir.path(), Arc::clone(&grants))
                .expect("the workspace resolves"),
        );
        let mut announced = grants.subscribe();
        let target = elsewhere.path().join("outside.txt");
        let resolved = fs::canonicalize(elsewhere.path())
            .expect("resolve")
            .join("outside.txt");

        let write = tokio::spawn({
            let fs = Arc::clone(&fs);
            let target = target.clone();
            async move { fs.write("session-7", &target, "hello\n").await }
        });

        let request = timeout(PATIENCE, announced.recv())
            .await
            .expect("a grant is announced")
            .expect("the stream is open");
        assert_eq!(request.path, resolved);
        assert_eq!(request.session, "session-7");
        assert_eq!(grants.pending(), vec![request.clone()]);
        assert!(
            !target.exists(),
            "the file must not exist while the grant is pending"
        );

        assert!(grants.approve(request.id), "the grant is still pending");
        write
            .await
            .expect("the task finishes")
            .expect("the approved write happens");
        assert_eq!(fs::read_to_string(&target).expect("read back"), "hello\n");
        assert!(
            grants.pending().is_empty(),
            "an answered grant is not pending"
        );
        assert!(
            !grants.approve(request.id),
            "a decision is delivered once, not twice"
        );
    }

    #[tokio::test]
    async fn a_denied_write_leaves_the_file_alone() {
        let dir = workspace();
        let elsewhere = workspace();
        let grants = Arc::new(Grants::with_timeout(None));
        let fs = Arc::new(
            Fs::with_grants(Arc::new(LocalHost), dir.path(), Arc::clone(&grants))
                .expect("the workspace resolves"),
        );
        let mut announced = grants.subscribe();
        let target = elsewhere.path().join("outside.txt");

        let write = tokio::spawn({
            let fs = Arc::clone(&fs);
            let target = target.clone();
            async move { fs.write("session-1", &target, "hello\n").await }
        });

        let request = timeout(PATIENCE, announced.recv())
            .await
            .expect("a grant is announced")
            .expect("the stream is open");
        assert!(grants.deny(request.id));

        let error = write
            .await
            .expect("the task finishes")
            .expect_err("a denial is an error");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert_eq!(error.code(), ErrorCode::RequestCancelled);
        assert!(!target.exists(), "a denied write writes nothing");
    }

    #[tokio::test]
    async fn a_write_with_nobody_listening_is_refused_instead_of_hanging() {
        let dir = workspace();
        let elsewhere = workspace();
        let fs = client(dir.path());
        let target = elsewhere.path().join("outside.txt");

        let error = timeout(PATIENCE, fs.write("session-1", &target, "hello\n"))
            .await
            .expect("the refusal is immediate")
            .expect_err("nothing can approve it");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert!(!target.exists());
        assert!(fs.grants().pending().is_empty());
    }

    #[tokio::test]
    async fn a_grant_nobody_answers_expires() {
        let dir = workspace();
        let elsewhere = workspace();
        let grants = Arc::new(Grants::with_timeout(Some(Duration::from_millis(50))));
        let fs = Fs::with_grants(Arc::new(LocalHost), dir.path(), Arc::clone(&grants))
            .expect("the workspace resolves");
        // Held so the request is published; never read.
        let _announced = grants.subscribe();
        let target = elsewhere.path().join("outside.txt");

        let error = timeout(PATIENCE, fs.write("session-1", &target, "hello\n"))
            .await
            .expect("the timeout ends the wait")
            .expect_err("nobody approved it");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert!(!target.exists());
        assert!(
            grants.pending().is_empty(),
            "an expired grant is not still pending"
        );
    }
}
