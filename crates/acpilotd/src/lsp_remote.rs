//! A language server on a host, driven through a helper process of its own.
//!
//! A language server is a conversation with two directions and no request that owns
//! the output: the client sends `initialize`, the server answers, and then it speaks
//! when it wants to (`publishDiagnostics`, progress, its own questions). That is the
//! same shape as a terminal, so it uses the same helper pattern
//! ([`crate::pty_remote`]): one helper process per server, a reader thread that sorts
//! replies from output, and bytes carried as base64 because a JSON string cannot hold a
//! `Content-Length` frame unchanged.
//!
//! The daemon's LSP client ([`acpilot_lsp::client::Server`]) is given the two halves of
//! an in-process pipe: it writes LSP frames into one, and reads the server's bytes from
//! the other. A bridge moves those bytes — for the host, `lsp.write`; for the daemon,
//! `lsp.data` — so the protocol above the pipe is the same code that talks to a server
//! on this machine, and an SSH host does not get a second, subtly different client.
//!
//! # Why the server runs on the host
//!
//! A language server indexes files. Running it here and sending text would mean every
//! file it wanted to read crossed the transport, and a workspace the daemon cannot see
//! has no text to send. The server indexes the workspace it was started in — the host's
//! spelling of the root — and answers with the host's paths, which is exactly what the
//! workspace's rows are.

use std::io::{BufRead, BufReader as SyncBufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use acpilot_lsp::{Server, Session};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::base64;
use crate::helper::{Connections, Transport};

/// How much of the server's output one bridge read carries.
const CHUNK: usize = 16 * 1024;

/// How long the helper is given to stop its server before it is killed outright.
///
/// A close is a request and a wait; a helper that is not gone by then is wedged, and
/// leaving it is worse than ending it. Twenty ticks of a tenth of a second.
const GRACE: Duration = Duration::from_millis(100);
const GRACE_TICKS: u32 = 20;

/// The far end of one remote session: what closes it when the daemon is done.
///
/// The [`Session`] is separate on purpose — the registry asks it questions and calls its
/// own `shutdown` — and this is the part that owns the helper process. Dropping it is
/// how a session that failed, or was stopped for being idle, stops costing the host a
/// process.
pub(crate) struct RemoteHandle {
    stop: mpsc::UnboundedSender<()>,
    bridge: tokio::task::JoinHandle<()>,
    child: Arc<Mutex<Child>>,
}

impl std::fmt::Debug for RemoteHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteHandle")
            .finish_non_exhaustive()
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        // Ending the bridge drops the pipe, which drops the helper's stdin: the helper
        // reads EOF and kills the servers it started, the same way it ends a terminal.
        // Killing the helper here instead would be a `SIGKILL` it cannot clean up after.
        let _ = self.stop.send(());
        self.bridge.abort();
        let child = Arc::clone(&self.child);
        std::thread::spawn(move || {
            for _ in 0..GRACE_TICKS {
                let ended = child
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .try_wait();
                match ended {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) => std::thread::sleep(GRACE),
                }
            }
            let mut child = child.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// Start `program` on the host and complete the LSP handshake.
///
/// `root` is the workspace root as the host spells it: the server indexes that path, and
/// every location it answers with is on that machine. `program` and `args` are the
/// detected server's, unchanged — which server a language has is the same decision on
/// both ends, and the helper only carries the bytes.
///
/// # Errors
///
/// The transport would not start, the helper speaks another protocol, the helper has no
/// `lsp.start`, the host refused to start the program, or the server did not answer
/// `initialize`. The message is the helper's or the server's own words where there are
/// any, so the route can say which side refused.
pub(crate) async fn start(
    connections: Arc<Connections>,
    transport: Transport,
    program: &str,
    args: Vec<String>,
    root: String,
) -> Result<(Session, RemoteHandle), String> {
    let wanted = program.to_owned();
    let on_host = root.clone();
    let (child, stdin, stdout, server) = tokio::task::spawn_blocking(move || {
        open(&connections, &transport, &wanted, &args, &on_host)
    })
    .await
    .map_err(|error| format!("the host's helper task failed: {error}"))??;

    let child = Arc::new(Mutex::new(child));
    let (to_helper, to_server) = tokio::io::duplex(256 * 1024);
    let (helper_read, helper_write) = tokio::io::split(to_helper);
    let (server_read, server_write) = tokio::io::split(to_server);

    // The helper's answers: one line is one notification, and only `lsp.data` is the
    // server; `lsp.stderr` is the host's log and `lsp.exit` is the end.
    let (data_tx, mut data) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("acpilot-lsp-remote-{server}"))
        .stack_size(128 * 1024)
        .spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match value.get("method").and_then(Value::as_str) {
                    Some("lsp.data") => {
                        let Some(encoded) = value["params"]["data"].as_str() else {
                            continue;
                        };
                        if let Ok(bytes) = base64::decode(encoded)
                            && data_tx.send(bytes).is_err()
                        {
                            return;
                        }
                    }
                    // A notification this build does not know is a newer helper's; the
                    // protocol above the pipe does not need it.
                    Some("lsp.exit") => return,
                    Some(_) => {}
                    None => {}
                }
            }
        })
        .map_err(|error| format!("{program}: {error}"))?;

    // The helper's stdin: a thread rather than the bridge, because `ChildStdin` is a
    // blocking pipe and a bridge task is not allowed to block. Ordering is the point —
    // LSP frames are a stream, and a write task per frame could reorder them.
    let (frames, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut helper_stdin: ChildStdin = stdin;
    std::thread::Builder::new()
        .name(format!("acpilot-lsp-out-{server}"))
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut next = 2u64;
            while let Some(bytes) = outgoing.blocking_recv() {
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": next,
                    "method": "lsp.write",
                    "params": { "id": server, "data": base64::encode(&bytes) }
                });
                next += 1;
                if writeln!(helper_stdin, "{request}")
                    .and_then(|()| helper_stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            // The caller is done: ask the helper to stop the server itself, which is the
            // close the helper can reap. A helper that has already gone needs nothing.
            let close = json!({
                "jsonrpc": "2.0",
                "id": next,
                "method": "lsp.close",
                "params": { "id": server }
            });
            let _ = writeln!(helper_stdin, "{close}").and_then(|()| helper_stdin.flush());
        })
        .map_err(|error| format!("{program}: {error}"))?;

    let (stop, mut stopped) = mpsc::unbounded_channel::<()>();
    let bridge = tokio::spawn(async move {
        let mut helper_read = helper_read;
        let mut helper_write = helper_write;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            tokio::select! {
                read = helper_read.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if frames.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                },
                chunk = data.recv() => match chunk {
                    Some(bytes) => {
                        if helper_write.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = stopped.recv() => break,
            }
        }
        // Dropping `frames` is what ends the writer thread; dropping `helper_write`
        // ends the server's input. Both are how the far side learns the conversation is
        // over without a signal.
        drop(helper_write);
        drop(frames);
    });

    let server = Server::attach(BufReader::new(server_read), server_write);
    match Session::attach(server, program, Path::new(&root)).await {
        Ok(session) => Ok((
            session,
            RemoteHandle {
                stop,
                bridge,
                child,
            },
        )),
        Err(error) => {
            let _ = stop.send(());
            bridge.abort();
            let mut child = child.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("{program} on the host: {error}"))
        }
    }
}

/// The blocking half: install ladder, handshake, `lsp.start`, and the pipes.
fn open(
    connections: &Connections,
    transport: &Transport,
    program: &str,
    args: &[String],
    root: &str,
) -> Result<(Child, ChildStdin, SyncBufReader<ChildStdout>, u32), String> {
    let mut session = connections.session(transport)?;
    let handshake = match session.hello() {
        Ok(handshake) => handshake,
        Err(error) => {
            session.end();
            return Err(error);
        }
    };
    if !offers(&handshake, "lsp.start") {
        session.end();
        return Err(format!(
            "the helper on {} does not start language servers",
            transport.address
        ));
    }
    let started = match session.request(
        1,
        "lsp.start",
        json!({ "program": program, "args": args, "cwd": root }),
    ) {
        Ok(started) => started,
        Err(error) => {
            session.end();
            return Err(error);
        }
    };
    let server = started
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| format!("the helper answered lsp.start with {started}, which has no id"))?;
    let (child, stdin, stdout) = session.into_parts();
    Ok((
        child,
        stdin,
        SyncBufReader::new(stdout.into_inner()),
        server,
    ))
}

/// Whether the handshake named a method.
fn offers(handshake: &Value, method: &str) -> bool {
    handshake
        .get("methods")
        .and_then(Value::as_array)
        .is_some_and(|methods| {
            methods
                .iter()
                .filter_map(Value::as_str)
                .any(|offered| offered == method)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_helper_from_before_language_servers_is_refused_by_name() {
        let handshake = json!({ "methods": ["hello", "fs.read", "exec", "pty.open"] });
        assert!(!offers(&handshake, "lsp.start"));
        let newer = json!({ "methods": ["hello", "lsp.start", "lsp.write", "lsp.close"] });
        assert!(offers(&newer, "lsp.start"));
    }
}
