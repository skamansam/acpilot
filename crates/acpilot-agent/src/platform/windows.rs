//! Windows: no console window, and a tree kill through `taskkill /T`.
//!
//! A job object is the right long-term answer — it is the only way to guarantee
//! that a grandchild cannot escape between the kill and the next spawn — but
//! assigning one needs the `windows` crate's `unsafe` calls, and this workspace
//! forbids unsafe code. `taskkill /T /F` reaches the same tree with a safe API;
//! The engine phase's task P2-5 owns replacing it when the terminal layer needs the stronger guarantee.
//!
//! `CREATE_NO_WINDOW` is spelled as a number rather than imported so this crate
//! does not link the `windows` crate for one constant.

use std::io;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Do not allocate a console for the child. An agent speaks ACP over pipes; a
/// console window would flash on every spawn.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub(crate) fn configure(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

pub(crate) fn kill_tree(child: &mut Child) -> io::Result<()> {
    // Best effort: if `taskkill` itself is unavailable, the direct kill below
    // still ends the process the caller started.
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match child.kill() {
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

pub(crate) fn find_program(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.extension().is_some() || program.contains('\\') || program.contains('/') {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }

    let search = std::env::var_os("PATH")?;
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    for directory in std::env::split_paths(&search) {
        for extension in extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let path = directory.join(format!("{program}{extension}"));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}
