//! Language servers on the host.
//!
//! A language server is a conversation, not a call: the client sends requests, the
//! server sends answers and also speaks on its own (`publishDiagnostics`), and neither
//! side knows when the other will write. That is the same shape as a terminal, so it
//! uses the same second line shape this protocol already has — a **notification**
//! (`lsp.data`, `lsp.stderr`, `lsp.exit`) beside the responses that carry an `id`.
//!
//! # Why pipes and not a PTY
//!
//! LSP over stdio is already framed (`Content-Length`), and a PTY would add echo, CRLF
//! translation and terminal query sequences the protocol never asked for. The design
//! note in `docs/notes/ssh-agent.md` says the same thing from the other side: a PTY
//! adds bytes the client would have to parse out again.
//!
//! # Why the id is this process's
//!
//! As with `pty.*`, the id in `lsp.start`'s answer is assigned here. A pid means
//! nothing on a host whose process table the daemon cannot see, and it is reused by
//! the kernel; an id is a key both ends spell the same way.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Failure;
use crate::base64;
use crate::output::Output;

/// How much one `lsp.data` notification carries.
///
/// One read is one message in the daemon's pipe, so this is a buffer size rather than a
/// frame: large enough that a diagnostics publish is a handful of lines, small enough
/// that a huge index is not held in memory before it moves.
const READ: usize = 8192;

/// How often the waiter looks at a server that has not ended.
///
/// A language server can run for hours; this is one `try_wait` per server per tick, and
/// it is what turns the process ending into the `lsp.exit` line the daemon needs.
const POLL: Duration = Duration::from_millis(50);

/// One running server.
struct Server {
    /// `None` after [`Sessions::close`]: the pipe is what the program reads, and closing
    /// it is how a server is asked to stop.
    stdin: Option<ChildStdin>,
    /// Shared with the waiter thread, which is the only thing that reaps it.
    child: Arc<Mutex<Child>>,
}

/// Every language server this helper owns, and the pipe it reports them on.
pub struct Sessions {
    servers: Mutex<BTreeMap<u32, Server>>,
    /// The process's stdout, shared with every other session: two lines interleaved
    /// mid-JSON is a protocol that fails at random.
    out: Arc<Output>,
    next: AtomicU32,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sessions")
            .field(
                "running",
                &self.servers.lock().map(|map| map.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl Sessions {
    /// No server has been started yet.
    #[must_use]
    pub fn new(out: Arc<Output>) -> Self {
        Self {
            servers: Mutex::new(BTreeMap::new()),
            out,
            next: AtomicU32::new(1),
        }
    }

    /// Write one JSON line to the daemon, response or notification.
    ///
    /// A failed write is not reported: the daemon is the only reader, so a closed pipe
    /// means it is gone and the next read of stdin ends the process anyway.
    pub fn send(&self, value: &Value) {
        self.out.send(value);
    }

    /// Start `program` on the host and answer with the id it is known by.
    ///
    /// Nothing is sent to the server here: the caller's `initialize` is the first
    /// thing it sees, and this process only carries the bytes. `cwd` is the workspace
    /// root as the host spells it, because that is what a server indexes.
    pub fn start(self: &Arc<Self>, params: &Value) -> Result<Value, Failure> {
        let program = params
            .get("program")
            .and_then(Value::as_str)
            .filter(|program| !program.is_empty())
            .ok_or_else(|| Failure::params("`program` is required"))?;
        let mut command = Command::new(program);
        if let Some(args) = params.get("args").and_then(Value::as_array) {
            command.args(args.iter().filter_map(Value::as_str));
        }
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            command.current_dir(cwd);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Failure::host("the server has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Failure::host("the server has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Failure::host("the server has no stderr"))?;

        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let child = Arc::new(Mutex::new(child));
        self.servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Server {
                    stdin: Some(stdin),
                    child: Arc::clone(&child),
                },
            );

        // The output side. A read is one notification; the decoder on the other end is
        // the daemon's, and it sees exactly what the server wrote.
        let out = Arc::downgrade(self);
        let channel = id;
        std::thread::Builder::new()
            .name(format!("acpilot-lsp-out-{id}"))
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut stdout = stdout;
                let mut buffer = vec![0u8; READ];
                loop {
                    match stdout.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let Some(sessions) = out.upgrade() else {
                                return;
                            };
                            sessions.publish(
                                "lsp.data",
                                json!({ "id": channel, "data": base64::encode(&buffer[..read]) }),
                            );
                        }
                    }
                }
            })
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;

        // A server's stderr is its log, and refusing to read it is how that log becomes a
        // full pipe that blocks the server. It is forwarded rather than printed: the only
        // useful reader is the daemon's debug log, not the host's.
        let out = Arc::downgrade(self);
        std::thread::Builder::new()
            .name(format!("acpilot-lsp-err-{id}"))
            .stack_size(128 * 1024)
            .spawn(move || {
                let mut stderr = stderr;
                let mut buffer = vec![0u8; READ];
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let Some(sessions) = out.upgrade() else {
                                return;
                            };
                            sessions.publish(
                                "lsp.stderr",
                                json!({ "id": channel, "data": base64::encode(&buffer[..read]) }),
                            );
                        }
                    }
                }
            })
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;

        // The end. A server that exits is news the daemon cannot get any other way: its
        // pipe going quiet looks exactly like an index in progress.
        let out = Arc::downgrade(self);
        std::thread::Builder::new()
            .name(format!("acpilot-lsp-exit-{id}"))
            .stack_size(128 * 1024)
            .spawn(move || {
                let code = loop {
                    let ended = child
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .try_wait();
                    match ended {
                        Ok(Some(status)) => break status.code(),
                        // The waiter cannot see a child that was taken away, and a
                        // `try_wait` failure is the same news: nothing more to wait for.
                        Ok(None) => std::thread::sleep(POLL),
                        Err(_) => break None,
                    }
                };
                let Some(sessions) = out.upgrade() else {
                    return;
                };
                sessions
                    .servers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                sessions.publish("lsp.exit", json!({ "id": id, "code": code }));
            })
            .map_err(|error| Failure::host(format!("{program}: {error}")))?;

        eprintln!("acpilotd-helper: language server {id} ({program}) started");
        Ok(json!({ "id": id }))
    }

    /// Bytes for the server's stdin, base64 because a JSON string cannot carry a
    /// `Content-Length` frame's bytes and a caller's message may contain anything.
    pub fn write(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let data = params
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::params("`data` is required"))?;
        let bytes = base64::decode(data)
            .map_err(|error| Failure::params(format!("`data` is not base64: {error}")))?;
        let mut servers = self.servers.lock().unwrap_or_else(PoisonError::into_inner);
        let server = servers
            .get_mut(&id)
            .ok_or_else(|| Failure::host(format!("no language server {id}")))?;
        let stdin = server
            .stdin
            .as_mut()
            .ok_or_else(|| Failure::host(format!("language server {id} is closing")))?;
        stdin
            .write_all(&bytes)
            .and_then(|()| stdin.flush())
            .map_err(|error| Failure::host(format!("language server {id}: {error}")))?;
        Ok(json!({ "id": id, "bytes": bytes.len() }))
    }

    /// End a server: close its stdin, then insist.
    ///
    /// The protocol's own exit is two messages the daemon sends first; this is what
    /// happens after, and it is the same thing a closed terminal does to a shell.
    pub fn close(&self, params: &Value) -> Result<Value, Failure> {
        let id = id_param(params)?;
        let Some(server) = self
            .servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
        else {
            return Ok(json!({ "id": id, "closed": false }));
        };
        drop(server.stdin);
        let mut child = server.child.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = child.kill();
        let _ = child.wait();
        Ok(json!({ "id": id, "closed": true }))
    }

    /// End every server, because the daemon is gone.
    ///
    /// A server whose helper died is a process nobody can name or talk to; dropping the
    /// helper's stdin closes the pipe, which is the polite half, and this is the rest.
    pub fn close_all(&self) {
        let servers =
            std::mem::take(&mut *self.servers.lock().unwrap_or_else(PoisonError::into_inner));
        for (_, server) in servers {
            drop(server.stdin);
            let mut child = server.child.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// One notification, on the same locked stdout as responses.
    fn publish(&self, method: &str, params: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }
}

/// The id a call names, as a number.
fn id_param(params: &Value) -> Result<u32, Failure> {
    params
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| Failure::params("`id` is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_without_an_id_is_refused() {
        assert!(id_param(&json!({})).is_err());
        assert_eq!(id_param(&json!({ "id": 3 })).unwrap(), 3);
    }

    #[test]
    fn closing_an_unknown_server_says_so_rather_than_failing() {
        let sessions = Arc::new(Sessions::new(Arc::new(Output::new())));
        let answer = sessions.close(&json!({ "id": 99 })).unwrap();
        assert_eq!(answer, json!({ "id": 99, "closed": false }));
    }
}
