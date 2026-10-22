//! unix and macOS: the PTY is `forkpty`, which is what `portable-pty` does here.
//!
//! The construction lives here rather than in the shared handle so that the
//! handle has no `cfg` at all. That is the shape the boundary rule asks for: the
//! *difference* is in the platform file, and where there is no difference there
//! is no code. Nothing below is unix-specific beyond the call itself.

use crate::pty::{Pty, pty_size};
use crate::{ExecCommand, HostError, Size};

pub(crate) fn spawn_pty(command: &ExecCommand, size: Size) -> Result<Pty, HostError> {
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
    // exits, and the reader would never see EOF.
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

    Ok(Pty {
        master: pair.master,
        child,
        reader: Some(reader),
        writer,
        size,
    })
}
