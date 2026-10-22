//! The PTY handle both platforms share.
//!
//! `portable-pty` is cross-platform (`forkpty` on unix, ConPTY on Windows), so
//! the handle itself does not branch; only *creating* one does, and that lives in
//! `platform/`. Keeping the handle here means a caller can hold a `Pty` on any
//! platform without a `cfg` of its own — it simply cannot be handed one where
//! the platform cannot make one.

use std::fmt;
use std::io::{self, Read, Write};

use portable_pty::{MasterPty, PtySize};

use crate::{Exit, HostError, Size};

/// A running command attached to a pseudo-terminal.
pub struct Pty {
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(crate) reader: Option<Box<dyn Read + Send>>,
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) size: Size,
}

impl fmt::Debug for Pty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pty")
            .field("size", &self.size)
            .field("pid", &self.child.process_id())
            .finish_non_exhaustive()
    }
}

impl Pty {
    /// Take the output side. There is one reader per PTY: two consumers would
    /// each see part of the stream, which is a corrupted transcript rather than
    /// a shared one.
    pub fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader.take()
    }

    /// Send bytes to the terminal exactly as typed — no newline is added, because
    /// `text` vs `text\r` is the difference P0-2 measured.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Tell the terminal — and through it the program — that the window changed.
    pub fn resize(&mut self, size: Size) -> Result<(), HostError> {
        self.master
            .resize(pty_size(size))
            .map_err(|error| HostError::Pty {
                command: "resize".to_string(),
                message: error.to_string(),
            })?;
        self.size = size;
        Ok(())
    }

    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// The process on the far side, for a session record or a process-tree kill.
    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn wait(&mut self) -> Result<Exit, HostError> {
        let status = self.child.wait().map_err(|error| HostError::Pty {
            command: "wait".to_string(),
            message: error.to_string(),
        })?;
        Ok(exit_from_pty(status))
    }

    pub fn try_wait(&mut self) -> Result<Option<Exit>, HostError> {
        let status = self.child.try_wait().map_err(|error| HostError::Pty {
            command: "try_wait".to_string(),
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
