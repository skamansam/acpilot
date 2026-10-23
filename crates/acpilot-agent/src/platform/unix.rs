//! unix: a process group of the child's own, killed with one group signal.
//!
//! `process_group(0)` makes the child the leader of a new group whose id is its
//! pid. Everything it starts inherits that group, so killing `-pid` reaches the
//! wrapper and the agent behind it. Without this, `npx` exits and the node process
//! it started keeps the workspace's file handles and the stderr pipe open — which
//! is a session that looks dead and a reader thread that never sees EOF.

use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use rustix::process::{Pid, Signal, kill_process_group};

pub(crate) fn configure(command: &mut Command) {
    command.process_group(0);
}

pub(crate) fn kill_tree(child: &mut Child) -> io::Result<()> {
    if let Some(pid) = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX)) {
        // ESRCH means the group is already gone, which is the desired state.
        let _ = kill_process_group(pid, Signal::KILL);
    }
    // Fallback for the case where the group id was not ours to signal, and a
    // no-op double tap otherwise. An already-reaped child is not an error here:
    // the caller asked for it to be dead, and it is.
    match child.kill() {
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

pub(crate) fn find_program(program: &str) -> Option<PathBuf> {
    // A path with a separator is a path, not a name to search for.
    if program.contains('/') {
        let path = Path::new(program);
        return executable(path).then(|| path.to_path_buf());
    }

    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|directory| directory.join(program))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}
