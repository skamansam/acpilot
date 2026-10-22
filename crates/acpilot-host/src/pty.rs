//! The PTY handle every host shares, and the local way to make one.
//!
//! A terminal is not always a local `portable-pty`: an SSH session has one too,
//! and the daemon's terminal pane must not care which. So the handle delegates to
//! a [`Terminal`] backend, and each transport supplies one — `portable-pty` here
//! (`forkpty` on unix, ConPTY on Windows), an SSH channel in `ssh.rs`.
//!
//! The backend trait lives here rather than in `platform/` because it is not an
//! operating-system difference: two transports on the same OS differ more than
//! the same transport on two OSes. `platform/` still holds the `cfg` that names
//! an OS, and it still holds the local call site.

use std::fmt;
use std::io::{self, Read, Write};

use portable_pty::{MasterPty, PtySize};

use crate::{ExecCommand, Exit, HostError, Size};

/// What every transport must be able to do with a running terminal.
pub(crate) trait Terminal: Send {
    /// The output side, once. There is one reader per terminal: two consumers
    /// would each see part of the stream, which is a corrupted transcript rather
    /// than a shared one.
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>>;
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&mut self, size: Size) -> Result<(), HostError>;
    /// The process on the far side, when the transport can name one.
    fn process_id(&self) -> Option<u32>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> Result<Exit, HostError>;
    fn try_wait(&mut self) -> Result<Option<Exit>, HostError>;
}

/// A running command attached to a terminal, local or remote.
pub struct Pty {
    backend: Box<dyn Terminal>,
    size: Size,
}

impl fmt::Debug for Pty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pty")
            .field("size", &self.size)
            .field("pid", &self.backend.process_id())
            .finish_non_exhaustive()
    }
}

impl Pty {
    pub(crate) fn new(backend: Box<dyn Terminal>, size: Size) -> Self {
        Self { backend, size }
    }

    /// Take the output side. See [`Terminal::take_reader`].
    pub fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.backend.take_reader()
    }

    /// Send bytes to the terminal exactly as typed — no newline is added, because
    /// `text` vs `text\r` is the difference P0-2 measured.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.backend.write_all(bytes)
    }

    /// Tell the terminal — and through it the program — that the window changed.
    pub fn resize(&mut self, size: Size) -> Result<(), HostError> {
        self.backend.resize(size)?;
        self.size = size;
        Ok(())
    }

    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// The process on the far side, for a session record or a process-tree kill.
    /// `None` where the transport does not report one, which is the honest answer
    /// over SSH rather than a locally invented pid.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.backend.process_id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.backend.kill()
    }

    pub fn wait(&mut self) -> Result<Exit, HostError> {
        self.backend.wait()
    }

    pub fn try_wait(&mut self) -> Result<Option<Exit>, HostError> {
        self.backend.try_wait()
    }
}

/// Start `command` on a new local PTY, the platform's way.
///
/// unix: `forkpty` — the child is a session leader on a real `/dev/pts/N`.
/// Windows: ConPTY — the child is attached to a pseudo-console, and three things
/// differ visibly from unix: input is echoed, `\n` arrives as `\r\n`, and a
/// program that asks for the cursor position (`CSI 6 n`) blocks until something
/// answers. The answer is the terminal pane's job (P2-5), not this function's.
///
/// Shared rather than duplicated per platform because `portable-pty` already
/// owns the difference; duplicating the body would be two copies of the same
/// code with a Windows comment on one of them.
pub(crate) fn spawn(command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
    let pair = portable_pty::native_pty_system()
        .openpty(pty_size(size))
        .map_err(|error| HostError::Pty {
            command: command.command_line(),
            message: error.to_string(),
        })?;

    let mut builder = portable_pty::CommandBuilder::new(&command.program);
    for arg in &command.args {
        builder.arg(arg);
    }
    if let Some(cwd) = &command.cwd {
        builder.cwd(cwd);
    }
    for (key, value) in &command.env {
        builder.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| HostError::Pty {
            command: command.command_line(),
            message: error.to_string(),
        })?;
    // Holding the slave open would keep the terminal alive after the child
    // exits, and the reader would never see EOF. On Windows the slave is an
    // `Arc` clone of the pseudo-console, so dropping it here does not end the
    // session; the master does that when the `Pty` drops.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| HostError::Pty {
            command: command.command_line(),
            message: error.to_string(),
        })?;
    let writer = pair.master.take_writer().map_err(|error| HostError::Pty {
        command: command.command_line(),
        message: error.to_string(),
    })?;

    Ok(Pty::new(
        Box::new(LocalTerminal {
            master: pair.master,
            child,
            reader: Some(reader),
            writer,
            command: command.command_line(),
        }),
        size,
    ))
}

/// `portable-pty` behind [`Terminal`]: `forkpty` or ConPTY.
struct LocalTerminal {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
    /// For error messages that name the program rather than the seam.
    command: String,
}

impl Terminal for LocalTerminal {
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    fn resize(&mut self, size: Size) -> Result<(), HostError> {
        self.master
            .resize(pty_size(size))
            .map_err(|error| HostError::Pty {
                command: self.command.clone(),
                message: error.to_string(),
            })
    }

    fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    fn wait(&mut self) -> Result<Exit, HostError> {
        let status = self.child.wait().map_err(|error| HostError::Pty {
            command: self.command.clone(),
            message: error.to_string(),
        })?;
        Ok(exit_from_pty(status))
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, HostError> {
        let status = self.child.try_wait().map_err(|error| HostError::Pty {
            command: self.command.clone(),
            message: error.to_string(),
        })?;
        Ok(status.map(exit_from_pty))
    }
}

pub(crate) fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn exit_from_pty(status: portable_pty::ExitStatus) -> Exit {
    Exit {
        // A PTY exit is a code, never a signal: `portable-pty` reports the code
        // even when the process was killed.
        code: Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
        success: status.success(),
    }
}
