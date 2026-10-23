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
//! Whether a write lands is [`crate::permission`]'s decision, not this module's.
//! The shipped policy asks about a write inside the workspace and denies one
//! outside it, and a rule is how a workspace changes either answer. When the
//! answer is *ask*, the file does not change at all: a request is published on
//! the [`Broker`], the call waits for an approval or a denial, and the file is
//! written only after the approval. That is what makes it a *shadow* write —
//! while the request is pending the old contents are still the contents, so a UI
//! can show the diff against a file that has not moved, and a crash leaves the
//! disk as it was.
//!
//! Nothing is written speculatively and rolled back. Rolling back is a second
//! chance to write the wrong thing, and a file restored by a rollback is not the
//! same inode the editor had open.
//!
//! # Refusal is the default
//!
//! If nothing is listening, an asked request is refused immediately rather than
//! waiting for an approval that cannot arrive. If something is listening and
//! never answers, [`ASK_TIMEOUT`](crate::permission::ASK_TIMEOUT) ends the wait.
//! The agent gets a denial either way: a blocked agent with no path to a decision
//! is a hang, and a hang is what P2-9 found when an unimplemented method went
//! unanswered.
//!
//! # What this module is not
//!
//! Binary detection and the 10 MiB cap belong to the files API (P3-5); a file
//! that is not UTF-8 is refused here rather than converted. Reads outside the
//! workspace are refused structurally rather than by policy — a rule can deny a
//! read inside, but there is no approval flow that could ever allow one outside.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acpilot_host::{Host, HostError};
use acpilot_proto::permission::PermissionKind;
use agent_client_protocol::schema::v1::ErrorCode;
use thiserror::Error;

use crate::permission::{Broker, Policy, Subject};

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

    /// A write the policy refused, or one nobody approved.
    #[error("{} was not approved for writing", path.display())]
    Denied {
        /// The path that needed an approval.
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
    broker: Arc<Broker>,
}

impl Fs {
    /// Serve `workspace` on `host` under the shipped policy.
    ///
    /// The workspace is resolved through the host once, here, so every later
    /// check compares against the real path rather than whatever the caller
    /// spelled — and so the broker's "inside or outside" answer is about the same
    /// path a symlink actually leads to.
    pub fn new(host: Arc<dyn Host>, workspace: impl AsRef<Path>) -> Result<Self, FsError> {
        let workspace = workspace.as_ref();
        let resolved = host
            .canonicalize(workspace)
            .map_err(|source| FsError::Resolve {
                path: workspace.to_path_buf(),
                source,
            })?;
        let broker = Arc::new(Broker::new(Policy::shipped(), resolved.clone()));
        Ok(Self {
            host,
            workspace: resolved,
            broker,
        })
    }

    /// Serve files from a broker's workspace, under its policy.
    ///
    /// A daemon creates one [`Broker`] for the process and passes it to every
    /// session, so the pending list and the audit log are one list and one log.
    /// The broker's workspace is expected to be canonical, which [`Fs::new`] and
    /// [`Broker::new`] both ensure.
    #[must_use]
    pub fn with_broker(host: Arc<dyn Host>, broker: Arc<Broker>) -> Self {
        Self {
            host,
            workspace: broker.workspace().to_path_buf(),
            broker,
        }
    }

    /// The resolved workspace root.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The broker every request here is decided by.
    #[must_use]
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// Read a file inside the workspace, optionally one range of lines.
    ///
    /// `line` is 1-based and `limit` counts lines, as ACP defines them. Lines
    /// keep their terminators, so a range of a file reads back as text that can
    /// be written to another file unchanged.
    ///
    /// `session` is what makes the audit line say who read it.
    pub async fn read(
        &self,
        session: &str,
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
        let answer = self
            .broker
            .ask(
                session,
                Subject::new(
                    PermissionKind::FileRead,
                    format!("Read {}", label(&self.workspace, &real)),
                )
                .tool("fs/read_text_file")
                .path(&real)
                .detail(format!(
                    "{} (inside {})",
                    real.display(),
                    self.workspace.display()
                )),
            )
            .await;
        if !answer.allowed() {
            return Err(FsError::Denied { path: real });
        }
        let bytes = self.read_bytes(real.clone()).await?;
        slice(&real, &bytes, line, limit)
    }

    /// Write a file, if the policy allows it or somebody approves it.
    ///
    /// `session` is carried on the request so whoever approves it knows which
    /// conversation asked, and so an `allow always` is scoped to that
    /// conversation.
    pub async fn write(&self, session: &str, path: &Path, content: &str) -> Result<(), FsError> {
        let real = self.resolve_for_write(path)?;
        let answer = self
            .broker
            .ask(
                session,
                Subject::new(
                    PermissionKind::FileWrite,
                    format!("Write {}", label(&self.workspace, &real)),
                )
                .tool("fs/write_text_file")
                .path(&real)
                .detail(format!(
                    "{} ({} the workspace {})",
                    real.display(),
                    if real.starts_with(&self.workspace) {
                        "inside"
                    } else {
                        "outside"
                    },
                    self.workspace.display()
                )),
            )
            .await;
        if !answer.allowed() {
            return Err(FsError::Denied { path: real });
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

/// The path as a person reads it: relative to the workspace when it is inside
/// it, and the whole path when it is not.
fn label(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use acpilot_host::LocalHost;
    use acpilot_proto::permission::{PermissionOption, PermissionRequest};
    use tokio::sync::broadcast::Receiver;
    use tokio::time::timeout;

    use super::*;
    use crate::permission::{Broker, DecidedBy, Decision, Policy, Rule};

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn client(workspace: &Path) -> Fs {
        Fs::new(Arc::new(LocalHost), workspace).expect("the workspace resolves")
    }

    /// A client and the broker behind it, so a test can be the person answering.
    fn shared(dir: &Path, policy: Policy, timeout: Option<Duration>) -> (Arc<Fs>, Arc<Broker>) {
        let resolved = std::fs::canonicalize(dir).expect("the workspace resolves");
        let broker = Arc::new(Broker::with_timeout(policy, resolved, timeout));
        let fs = Arc::new(Fs::with_broker(Arc::new(LocalHost), Arc::clone(&broker)));
        (fs, broker)
    }

    /// Every wait in this module is bounded: a test that blocks forever is a test
    /// that reports nothing.
    const PATIENCE: Duration = Duration::from_secs(5);

    async fn announced(pending: &mut Receiver<PermissionRequest>) -> PermissionRequest {
        timeout(PATIENCE, pending.recv())
            .await
            .expect("a question is announced")
            .expect("the stream is open")
    }

    #[tokio::test]
    async fn a_file_in_the_workspace_is_read_from_disk() {
        let dir = workspace();
        fs::write(dir.path().join("notes.txt"), "one\ntwo\n").expect("write");
        let fs = client(dir.path());

        let content = fs
            .read("session-1", &dir.path().join("notes.txt"), None, None)
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
            .read("session-1", &path, Some(2), Some(2))
            .await
            .expect("the read happens");
        assert_eq!(range, "two\nthree\n");

        // Past the end is an empty answer, not an error: the file exists and the
        // agent asked for a window that does not overlap it.
        let beyond = fs
            .read("session-1", &path, Some(9), None)
            .await
            .expect("the read happens");
        assert_eq!(beyond, "");

        // A file with no trailing newline keeps that too.
        fs::write(&path, "only").expect("write");
        assert_eq!(
            fs.read("session-1", &path, None, None)
                .await
                .expect("the read happens"),
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

        let error = fs
            .read("session-1", &secret, None, None)
            .await
            .expect_err("refused");
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
            .read("session-1", &link, None, None)
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
            .read("session-1", Path::new("notes.txt"), None, None)
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
            .read("session-1", &dir.path().join("bytes.bin"), None, None)
            .await
            .expect_err("not text");
        assert!(matches!(error, FsError::NotText { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_file_says_which_one_and_why() {
        let dir = workspace();
        let fs = client(dir.path());

        let error = fs
            .read("session-1", &dir.path().join("absent.txt"), None, None)
            .await
            .expect_err("no such file");
        assert_eq!(error.code(), ErrorCode::ResourceNotFound);
        assert!(
            error.to_string().contains("absent.txt"),
            "the message names the file: {error}"
        );
    }

    #[tokio::test]
    async fn a_deny_rule_refuses_a_read_the_default_would_allow() {
        let dir = workspace();
        fs::write(dir.path().join(".env"), "TOKEN=1\n").expect("write");
        let (fs, broker) = shared(
            dir.path(),
            Policy::with_rules(vec![
                Rule::new("no-secrets", Decision::Deny).path("**/.env"),
            ]),
            None,
        );

        let error = fs
            .read("session-1", &dir.path().join(".env"), None, None)
            .await
            .expect_err("the rule refuses the read");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");

        let audit = broker.audit();
        assert_eq!(audit.len(), 1, "the refusal is on the record");
        assert_eq!(audit[0].kind, PermissionKind::FileRead);
        assert_eq!(
            audit[0].decided_by,
            DecidedBy::Policy("no-secrets".to_owned())
        );
    }

    #[tokio::test]
    async fn a_write_inside_the_workspace_lands_without_asking() {
        let dir = workspace();
        let (fs, broker) = shared(dir.path(), Policy::shipped(), None);
        let target = dir.path().join("written.txt");

        fs.write("session-7", &target, "hello\n")
            .await
            .expect("an edit inside the workspace needs no signature");
        assert_eq!(fs::read_to_string(&target).expect("read back"), "hello\n");

        let audit = broker.audit();
        assert_eq!(audit.len(), 1, "the write is on the record anyway");
        assert_eq!(audit[0].session, "session-7");
        assert_eq!(audit[0].kind, PermissionKind::FileWrite);
        assert_eq!(audit[0].option, PermissionOption::AllowOnce);
        assert_eq!(audit[0].decided_by, DecidedBy::Default);
        assert!(broker.pending().is_empty(), "nothing was left waiting");
    }

    #[tokio::test]
    async fn a_policy_that_asks_and_nobody_is_listening_refuses_at_once() {
        let dir = workspace();
        let (fs, broker) = shared(
            dir.path(),
            Policy::shipped().with_write(Decision::Ask),
            None,
        );
        let target = dir.path().join("written.txt");

        let error = timeout(PATIENCE, fs.write("session-1", &target, "hello\n"))
            .await
            .expect("the refusal is immediate")
            .expect_err("nothing can approve it");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert!(!target.exists());
        assert!(broker.pending().is_empty());
        assert_eq!(broker.audit()[0].decided_by, DecidedBy::Unattended);
    }

    #[tokio::test]
    async fn a_denied_write_leaves_the_file_alone() {
        let dir = workspace();
        let (fs, broker) = shared(
            dir.path(),
            Policy::shipped().with_write(Decision::Ask),
            None,
        );
        let mut pending = broker.subscribe();
        let target = dir.path().join("written.txt");

        let write = tokio::spawn({
            let fs = Arc::clone(&fs);
            let target = target.clone();
            async move { fs.write("session-1", &target, "hello\n").await }
        });

        let request = announced(&mut pending).await;
        assert!(broker.answer(&request.id, PermissionOption::Deny));

        let error = write
            .await
            .expect("the task finishes")
            .expect_err("a denial is an error");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert_eq!(error.code(), ErrorCode::RequestCancelled);
        assert!(!target.exists(), "a denied write writes nothing");
    }

    #[tokio::test]
    async fn a_policy_that_denies_outside_writes_writes_nothing() {
        let dir = workspace();
        let elsewhere = workspace();
        let (fs, broker) = shared(
            dir.path(),
            Policy::shipped().with_outside(Decision::Deny),
            None,
        );
        let _pending = broker.subscribe();
        let target = elsewhere.path().join("outside.txt");

        let error = fs
            .write("session-1", &target, "hello\n")
            .await
            .expect_err("outside is denied, not asked about");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert!(!target.exists());
        assert!(broker.pending().is_empty());
        assert_eq!(broker.audit()[0].decided_by, DecidedBy::Default);
    }

    #[tokio::test]
    async fn an_outside_write_waits_for_approval() {
        let dir = workspace();
        let elsewhere = workspace();
        let (fs, broker) = shared(dir.path(), Policy::shipped(), None);
        let mut pending = broker.subscribe();
        let target = elsewhere.path().join("outside.txt");

        let write = tokio::spawn({
            let fs = Arc::clone(&fs);
            let target = target.clone();
            async move { fs.write("session-1", &target, "hello\n").await }
        });

        let request = announced(&mut pending).await;
        assert!(
            request.detail.contains("outside"),
            "the person is told it is outside the workspace: {}",
            request.detail
        );
        assert!(
            !target.exists(),
            "the file must not exist while the request is pending"
        );
        assert_eq!(
            broker.audit().len(),
            0,
            "nothing is decided until somebody answers"
        );

        assert!(broker.answer(&request.id, PermissionOption::AllowOnce));
        write
            .await
            .expect("the task finishes")
            .expect("the approved write happens");
        assert_eq!(fs::read_to_string(&target).expect("read back"), "hello\n");
    }

    #[tokio::test]
    async fn a_request_nobody_answers_expires_and_writes_nothing() {
        let dir = workspace();
        let elsewhere = workspace();
        let (fs, broker) = shared(
            dir.path(),
            Policy::shipped(),
            Some(Duration::from_millis(50)),
        );
        // Held so the request is published; never read.
        let _pending = broker.subscribe();
        let target = elsewhere.path().join("outside.txt");

        let error = timeout(PATIENCE, fs.write("session-1", &target, "hello\n"))
            .await
            .expect("the timeout ends the wait")
            .expect_err("nobody approved it");
        assert!(matches!(error, FsError::Denied { .. }), "{error:?}");
        assert!(!target.exists());
        assert!(broker.pending().is_empty());
        assert_eq!(broker.audit()[0].decided_by, DecidedBy::Timeout);
    }
}
