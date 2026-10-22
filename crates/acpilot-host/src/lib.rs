//! Where files and processes actually live.
//!
//! Every filesystem and process operation in Acpilot is meant to go through this
//! trait, so that "on the remote box" and "in the WSL distro" are the same code
//! path as "on this laptop" — including language servers and agents, not just
//! editing. Windows→WSL deliberately goes through the helper binary rather than
//! `\\wsl.localhost` paths, which are slow enough to feel broken.
//!
//! ## Unix and macOS first, Windows last
//!
//! The project is built and tested on unix, so the unix implementation exists
//! first and the Windows one is written last — otherwise every Windows special
//! case is written blind and only ever exercised by a CI runner. The rule that
//! keeps that order honest is structural, not documentary:
//!
//! **`cfg(windows)` and `cfg(unix)` may appear only under `src/platform/`.**
//!
//! There is one file per platform, and both must satisfy the same
//! `platform::spawn_pty` contract; `tests/platform_boundary.rs` fails if a
//! branch appears anywhere else in any crate's sources, so a Windows special
//! case cannot spread through the host layer one `cfg!` at a time.
//!
//! Exec does not branch at all: `std::process` is portable, so `Host::spawn` has
//! exactly one implementation. Only the PTY differs, and on Windows it answers
//! [`HostError::Unsupported`] naming the phase that owes it — a refusal, not a
//! silent fallback to pipes.

mod platform;
mod pty;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Stdio};

pub use pty::Pty;

/// One command to run on a host, wherever that host is.
///
/// Arguments and environment are `OsString` because a path is not guaranteed to
/// be UTF-8, and silently lossy-converting one is how a file that exists becomes
/// a file that does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecCommand {
    /// Program to execute, resolved on the host — not on the machine calling in.
    pub program: OsString,
    /// Arguments, verbatim.
    pub args: Vec<OsString>,
    /// Working directory on the host. `None` inherits the host process's.
    pub cwd: Option<PathBuf>,
    /// Environment to add. Nothing is inherited twice; the host decides what the
    /// rest of its environment looks like.
    pub env: Vec<(OsString, OsString)>,
}

impl ExecCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// `program arg1 arg2`, for messages. Not a shell command line.
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut out = self.program.to_string_lossy().into_owned();
        for arg in &self.args {
            out.push(' ');
            out.push_str(&arg.to_string_lossy());
        }
        out
    }
}

/// A terminal size in cells. Pixels are deliberately absent: nothing in the
/// product has needed them, and a zero would be a lie where `None` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    /// What a program gets when nobody has said otherwise.
    pub const DEFAULT: Self = Self { cols: 80, rows: 24 };

    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl Default for Size {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How a host is reached. Matches the three transports the daemon offers, and
/// exists so a session record can say which one it used without carrying a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostKind {
    /// This machine.
    Local,
    /// A remote machine over SSH.
    Ssh,
    /// A WSL distro, reached through `wsl.exe` and the helper binary.
    Wsl,
}

impl fmt::Display for HostKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
            Self::Wsl => "wsl",
        })
    }
}

/// How a process ended.
///
/// `std::process::ExitStatus` and `portable_pty::ExitStatus` are different
/// types; the seam normalises both so callers do not branch on which side of the
/// port a process ran on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// Exit code where the platform reports one. `None` means a signal, which
    /// only unix reports — and which a supervisor must not read as success.
    pub code: Option<i32>,
    pub success: bool,
}

impl From<std::process::ExitStatus> for Exit {
    fn from(status: std::process::ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

/// A spawned process with piped stdin, stdout and stderr.
#[derive(Debug)]
pub struct Child {
    program: String,
    inner: std::process::Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

impl Child {
    /// The program as it was asked for, for error messages that name the agent
    /// rather than the daemon.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.inner.kill()
    }

    pub fn wait(&mut self) -> io::Result<Exit> {
        self.inner.wait().map(Exit::from)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        self.inner.try_wait().map(|status| status.map(Exit::from))
    }
}

/// What a host must be able to do for the daemon to run anything on it.
///
/// Three methods, because that is what P0-4 measured and what P2 needs first:
/// run a command with pipes, run a command under a PTY, and let the caller kill
/// either. File and LSP operations join this trait later, when their callers
/// exist — a trait invented ahead of its implementations is a guess with a
/// version number.
pub trait Host: fmt::Debug + Send + Sync {
    fn kind(&self) -> HostKind;

    /// Run a command with piped stdio. The caller owns the pipes and must read
    /// them, because a full pipe blocks the child.
    fn spawn(&self, command: &ExecCommand) -> Result<Child, HostError>;

    /// Run a command under a pseudo-terminal of the given size.
    ///
    /// A PTY is not "pipes with a nice name": programs detect it, colour their
    /// output, and refuse to prompt without it. On a platform that cannot yet
    /// make one this returns [`HostError::Unsupported`] rather than quietly
    /// handing back pipes, which is the failure mode that looks like a working
    /// terminal until someone opens a TUI.
    fn spawn_pty(&self, command: &ExecCommand, size: Size) -> Result<Pty, HostError>;
}

/// This machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalHost;

impl LocalHost {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Host for LocalHost {
    fn kind(&self) -> HostKind {
        HostKind::Local
    }

    fn spawn(&self, command: &ExecCommand) -> Result<Child, HostError> {
        let mut process = std::process::Command::new(&command.program);
        process
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }
        for (key, value) in &command.env {
            process.env(key, value);
        }

        let mut child = process.spawn().map_err(|source| HostError::Spawn {
            command: command.command_line(),
            source,
        })?;
        Ok(Child {
            program: command.program.to_string_lossy().into_owned(),
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            inner: child,
        })
    }

    fn spawn_pty(&self, command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
        platform::spawn_pty(command, size)
    }
}

/// Why a host operation did not happen.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The platform cannot do this yet, and the message says who owes it.
    ///
    /// A typed refusal, because the alternative — a fallback that returns
    /// something plausible — turns "not implemented" into "works on my machine".
    #[error("{feature} is not implemented on {platform} yet ({owner})")]
    Unsupported {
        feature: &'static str,
        platform: &'static str,
        owner: &'static str,
    },

    #[error("cannot spawn `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("pty for `{command}`: {message}")]
    Pty { command: String, message: String },

    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}
