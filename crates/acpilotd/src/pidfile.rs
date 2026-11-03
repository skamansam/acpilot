//! The pidfile: which process owns this endpoint, and how a second one finds out.
//!
//! The lock is the endpoint, not the file. A unix socket cannot be bound twice and
//! a TCP port cannot be listened on twice, so the kernel already refuses a second
//! daemon — what it does not give is a *reason*. This file is that reason: the pid,
//! the endpoint and the version of the process holding it, so the second daemon
//! says "pid 4123 is already listening on …" instead of `EADDRINUSE`.
//!
//! That is also why a leftover file is not a mystery: the endpoint is asked whether
//! anything answers on it, and a file whose endpoint is silent is stale and gets
//! replaced. There is no `flock` here on purpose — a lock file can be left held by a
//! process that crashed in a way the kernel did not clean up (a network filesystem,
//! a container boundary), and then the daemon cannot start at all. An endpoint is
//! exactly as durable as the thing it guards.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::platform::{Target, endpoint_answers};

/// What the file says about the process that owns it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    /// The owning process.
    pub pid: u32,
    /// The endpoint it is serving, as [`Target`] displays it.
    pub endpoint: String,
    /// The daemon version, so an upgrade mismatch is visible in the message.
    pub version: String,
    /// When it started, RFC 3339.
    pub started_at: String,
}

/// A pidfile this process owns.
///
/// The file is removed when this is dropped: a daemon that exits cleanly should not
/// leave a record saying otherwise. A daemon that is killed leaves one, and the next
/// start replaces it because its endpoint no longer answers.
#[derive(Debug)]
pub struct PidFile {
    path: PathBuf,
    pid: u32,
}

impl PidFile {
    /// The file this process owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        // Only ours to remove: a daemon that was refused saw *somebody else's* file,
        // and deleting it on the way out would delete the running daemon's record.
        if let Ok(text) = std::fs::read_to_string(&self.path)
            && let Ok(record) = serde_json::from_str::<Record>(&text)
            && record.pid == self.pid
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Claim `path` for this process, or say who already has it.
///
/// # Errors
///
/// Another daemon answers on `target`, or the file could not be written.
pub fn acquire(path: &Path, target: &Target) -> Result<PidFile, PidError> {
    if let Some(directory) = path.parent()
        && !directory.as_os_str().is_empty()
    {
        std::fs::create_dir_all(directory).map_err(|source| PidError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        acpilot_host::private_dir(directory).map_err(|source| PidError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
    }
    let pid = std::process::id();
    let record = Record {
        pid,
        endpoint: target.to_string(),
        version: acpilot_core::VERSION.to_owned(),
        started_at: jiff::Timestamp::now().to_string(),
    };
    let body = serde_json::to_string(&record).map_err(PidError::Json)?;

    // Two attempts: the second is the one that replaces a stale file, and a file
    // that appears between the two is somebody else's race, so it loses.
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(format!("{body}\n").as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|source| PidError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                return Ok(PidFile {
                    path: path.to_path_buf(),
                    pid,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                if let Some(existing) = read(path)
                    && endpoint_answers(&parse(&existing.endpoint))
                {
                    return Err(PidError::Running {
                        pid: existing.pid,
                        endpoint: existing.endpoint,
                    });
                }
                // Stale: the process that wrote it is not answering. Its socket may
                // still be on disk, which is the transport's problem to clear.
                std::fs::remove_file(path).map_err(|source| PidError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(PidError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    // The loop either returned or replaced on its one retry, so reaching here means
    // a file appeared twice, which is another daemon starting at the same moment.
    Err(PidError::Running {
        pid: 0,
        endpoint: target.to_string(),
    })
}

/// What the file says, when it says anything readable.
fn read(path: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Rebuild the endpoint a record names.
///
/// [`Target`]'s `Display` is the inverse: a `http://` prefix means TCP, anything
/// else is a socket path.
fn parse(endpoint: &str) -> Target {
    match endpoint.strip_prefix("http://") {
        Some(addr) => Target::Tcp(addr.to_owned()),
        None => Target::Socket(PathBuf::from(endpoint)),
    }
}

/// Why a pidfile could not be claimed.
#[derive(Debug, thiserror::Error)]
pub enum PidError {
    /// A daemon is already listening on the endpoint.
    #[error(
        "pid {pid} is already serving {endpoint}; one daemon per endpoint, and this one \
         refuses to start beside it"
    )]
    Running {
        /// The owning process.
        pid: u32,
        /// The endpoint it holds.
        endpoint: String,
    },

    /// The file could not be created, written or removed.
    #[error("cannot write {}: {source}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },

    /// The record could not be encoded, which is a bug rather than a condition.
    #[error("cannot encode the pidfile: {0}")]
    Json(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port something is listening on, and the listener keeping it open.
    ///
    /// TCP rather than a unix socket so this file names no operating system: the
    /// platform boundary is a crate rule, and pidfile's tests do not need a branch
    /// of their own to test the endpoint question. A listener completes the
    /// handshake from its backlog even before it accepts, which is what a bound
    /// daemon looks like from outside.
    fn live_endpoint() -> (Target, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("an address");
        (Target::Tcp(addr.to_string()), listener)
    }

    /// A port nothing is listening on: bound once, then dropped.
    fn silent_endpoint() -> Target {
        let (target, listener) = live_endpoint();
        drop(listener);
        target
    }

    #[test]
    fn a_second_daemon_on_a_live_endpoint_is_refused_and_names_the_first() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (target, listener) = live_endpoint();
        let file = directory.path().join("acpilotd.pid");
        let first = acquire(&file, &target).expect("the first");
        assert_eq!(read(&file).expect("a record").pid, std::process::id());

        let error = acquire(&file, &target).expect_err("the second");
        match error {
            PidError::Running { pid, .. } => assert_eq!(pid, std::process::id()),
            other => panic!("expected a running daemon, got {other}"),
        }
        drop(listener);
        drop(first);
    }

    #[test]
    fn a_pidfile_left_by_a_dead_daemon_is_replaced() {
        let directory = tempfile::tempdir().expect("temp dir");
        // An endpoint nothing answers on: the file names a daemon that is gone,
        // which is what a crash leaves behind.
        let target = silent_endpoint();
        let file = directory.path().join("acpilotd.pid");
        std::fs::write(
            &file,
            serde_json::to_string(&Record {
                pid: 999_999,
                endpoint: target.to_string(),
                version: "0.0.0".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
            })
            .expect("record"),
        )
        .expect("write");

        let claimed = acquire(&file, &target).expect("the stale file is replaced");
        assert_eq!(read(&file).expect("a record").pid, std::process::id());
        drop(claimed);
    }

    #[test]
    fn dropping_the_pidfile_removes_ours_and_leaves_somebody_elses() {
        let directory = tempfile::tempdir().expect("temp dir");
        let file = directory.path().join("acpilotd.pid");
        let target = Target::Tcp("127.0.0.1:9".into());
        let claimed = acquire(&file, &target).expect("acquire");
        assert!(file.exists());
        drop(claimed);
        assert!(!file.exists(), "a clean exit leaves no record");

        std::fs::write(
            &file,
            serde_json::to_string(&Record {
                pid: 1,
                endpoint: target.to_string(),
                version: "0.0.0".into(),
                started_at: "2026-01-01T00:00:00Z".into(),
            })
            .expect("record"),
        )
        .expect("write");
        let ours = PidFile {
            path: file.clone(),
            pid: 424_242,
        };
        drop(ours);
        assert!(file.exists(), "the file belongs to pid 1, not to the drop");
    }
}
