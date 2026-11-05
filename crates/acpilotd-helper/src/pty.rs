//! Terminals on the host.
//!
//! A PTY is not a call. It produces bytes before and after the request that opened
//! it, and a request/response protocol has nowhere to put them — so the same stdio
//! pipe carries two shapes of line: a **response**, which has an `id`, and a
//! **notification**, which has a `method` (`pty.data`, `pty.exit`). The daemon opens
//! one helper process per terminal, so a stream never queues behind a request or
//! behind another terminal's output.
//!
//! The PTY itself is `portable-pty`, the same crate the daemon's own host uses: the
//! platform branch (`forkpty`, ConPTY) is inside it, and a helper that opened
//! `/dev/ptmx` by hand would be `unsafe`, which the workspace forbids.
//!
//! # Why the id is the helper's, not the program's
//!
//! `pty.open` answers with an id this process assigns, and every later call names
//! that id rather than a pid. A pid is reused by the kernel and means nothing on a
//! host whose process table the daemon does not see; an id is a key both ends
//! spell the same way, which is the only thing a notification can be routed by.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{Value, json};

use crate::Failure;
use crate::base64;

/// How much one `pty.data` notification carries.
///
/// One read is one frame on the daemon's side, for the same reason its own terminal
/// reads in kilobytes: a pane that redraws forty times for one screenful flickers.
const READ: usize = 8192;

/// One running program and the master side of its terminal.
struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

/// Every terminal this helper owns, and the pipe it reports them on.
pub struct Sessions {
    terminals: Mutex<BTreeMap<u32, Terminal>>,
    /// The one stdout, behind a lock: a response and a notification are written by
    /// different threads, and two lines interleaved mid-JSON would be a protocol that
    /// fails at random.
    out: Mutex<io::Stdout>,
    next: AtomicU32,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sessions")
            .field(
                "open",
                &self.terminals.lock().map(|map| map.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl Sessions {
    /// No terminal has been opened yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            terminals: Mutex::new(BTreeMap::new()),
            out: Mutex::new(io::stdout()),
            next: AtomicU32::new(0),
        }
    }

    /// Write one JSON line to the daemon.
    ///
    /// A failed write is not reported: the daemon is the only reader, so a closed pipe
    /// means it is gone, and the next read of stdin ends the process anyway.
    pub fn send(&self, value: &Value) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(out, "{value}").and_then(|()| out.flush());
    }

    /// Start a program on a new terminal.
    ///
    /// `program` absent means the host's own shell: `$SHELL` where the login set one,
    /// `/bin/sh` otherwise. The daemon does not choose it, because the daemon's idea
    /// of a shell is about the machine it runs on, not the one the files are on.
    pub fn open(self: &Arc<Self>, params: &Value) -> Result<Value, Failure> {
        let cols = dimension(params, "cols")?;
        let rows = dimension(params, "rows")?;
        let program = params
            .get("program")
            .and_then(Value::as_str)
            .filter(|program| !program.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                std::env::var("SHELL")
                    .ok()
                    .filter(|shell| !shell.is_empty())
            })
            .unwrap_or_else(|| "/bin/sh".to_owned());
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;
        let mut builder = CommandBuilder::new(&program);
        if let Some(args) = params.get("args").and_then(Value::as_array) {
            for arg in args.iter().filter_map(Value::as_str) {
                builder.arg(arg);
            }
        }
        if let Some(cwd) = params
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
        {
            builder.cwd(cwd);
        }
        // A program that asks what terminal it is has to get an answer: the daemon may
        // have no `TERM` at all (it is often started by a service manager), and a shell
        // that believes it is not a terminal drops colour and line editing. Set before
        // the request's own environment, so an explicit `TERM` wins.
        builder.env("TERM", "xterm-256color");
        if let Some(env) = params.get("env").and_then(Value::as_object) {
            for (key, value) in env {
                if let Some(value) = value.as_str() {
                    builder.env(key, value);
                }
            }
        }
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;
        // Holding the slave open would keep the terminal alive after the program
        // exits, and the reader would never see EOF.
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;
        let pid = child.process_id();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Terminal {
                    master: pair.master,
                    writer,
                    child,
                },
            );

        // The reader is the terminal's voice: every byte the program writes is one
        // notification, and the exit is the last one. It is a thread rather than a
        // poll because the daemon needs the bytes as they arrive, not at the next
        // time it happens to ask.
        let sessions = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("acpilot-helper-pty-{id}"))
            .stack_size(128 * 1024)
            .spawn(move || pump(sessions, id, reader))
            .map_err(|error| {
                let _ = self
                    .terminals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Failure::host(format!("{program}: {error}"))
            })?;

        Ok(json!({ "id": id, "pid": pid, "program": program }))
    }

    /// Bytes as if typed. No newline is added: `text` and `text\r` differ, and a shell
    /// that receives the wrong one runs a command nobody asked for.
    pub fn write(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let data = params
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::params("`data` is required"))?;
        let bytes = base64::decode(data).map_err(Failure::params)?;
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let terminal = terminals
            .get_mut(&id)
            .ok_or_else(|| Failure::host(format!("no terminal {id}")))?;
        terminal
            .writer
            .write_all(&bytes)
            .and_then(|()| terminal.writer.flush())
            .map_err(|error| Failure::host(format!("terminal {id}: {error}")))?;
        Ok(json!({ "id": id, "bytes": bytes.len() }))
    }

    /// Tell the program its window changed shape.
    pub fn resize(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let cols = dimension(params, "cols")?;
        let rows = dimension(params, "rows")?;
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let terminal = terminals
            .get_mut(&id)
            .ok_or_else(|| Failure::host(format!("no terminal {id}")))?;
        terminal
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| Failure::host(format!("terminal {id}: {error}")))?;
        Ok(json!({ "id": id, "cols": cols, "rows": rows }))
    }

    /// End the program.
    ///
    /// The entry stays until the reader sees EOF, which is what carries the exit code:
    /// a `pty.exit` with no code would be a pane that cannot say how the command it
    /// watched ended.
    pub fn kill(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let terminal = terminals
            .get_mut(&id)
            .ok_or_else(|| Failure::host(format!("no terminal {id}")))?;
        terminal
            .child
            .kill()
            .map_err(|error| Failure::host(format!("terminal {id}: {error}")))?;
        Ok(json!({ "id": id, "signalled": true }))
    }

    /// End the program and forget it.
    ///
    /// The same signal as [`Sessions::kill`] plus the entry going away, for a daemon
    /// that is done with the terminal and would rather not wait for the exit
    /// notification it is about to stop reading. A later `pty.exit` for this id
    /// carries no code, because the process table entry was reaped here.
    pub fn close(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(mut terminal) = terminals.remove(&id) else {
            return Ok(json!({ "id": id, "closed": false }));
        };
        let _ = terminal.child.kill();
        let _ = terminal.child.wait();
        Ok(json!({ "id": id, "closed": true }))
    }

    /// End every terminal, when the daemon's pipe closes.
    ///
    /// A shell whose helper died is a process nobody can name or attach to; the master
    /// closing hangs it up, which is the same thing a closed terminal window does.
    pub fn close_all(&self) {
        let mut terminals = self
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (_, mut terminal) in std::mem::take(&mut *terminals) {
            let _ = terminal.child.kill();
            let _ = terminal.child.wait();
        }
    }
}

/// Read one terminal until it ends, and tell the daemon about both.
fn pump(sessions: Arc<Sessions>, id: u32, mut reader: Box<dyn Read + Send>) {
    let mut buffer = [0u8; READ];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => sessions.send(&json!({
                "jsonrpc": "2.0",
                "method": "pty.data",
                "params": { "id": id, "data": base64::encode(&buffer[..read]) },
            })),
            // A signal interrupted the read; the terminal is still there.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // `EIO` on a closed PTY is EOF on unix, and there is nothing else a read
            // failure can mean here: the exit status below is the real answer.
            Err(_) => break,
        }
    }
    let code = {
        let mut terminals = sessions
            .terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match terminals.remove(&id) {
            Some(mut terminal) => match terminal.child.wait() {
                Ok(status) => i32::try_from(status.exit_code()).ok(),
                Err(_) => None,
            },
            // Closed by the daemon while the reader was still draining: the code was
            // reaped there, and saying `null` is honest about it.
            None => None,
        }
    };
    sessions.send(&json!({
        "jsonrpc": "2.0",
        "method": "pty.exit",
        "params": { "id": id, "code": code },
    }));
}

fn id_param(params: &Value) -> Result<u32, Failure> {
    params
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| Failure::params("`id` is required and is the one `pty.open` answered"))
}

fn dimension(params: &Value, name: &str) -> Result<u16, Failure> {
    params
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| Failure::params(format!("`{name}` is required and must be above zero")))
}
