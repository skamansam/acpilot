//! A terminal on a host, driven through its own helper process.
//!
//! The files connection in [`crate::helper`] is one conversation per host: a request
//! goes out, one line comes back, and the next request starts from that clean state.
//! A terminal cannot work that way — its output arrives whenever the program decides
//! to print, which may be between two requests or during one — so a terminal gets a
//! helper process of its own and a reader thread that sorts what comes back:
//!
//! - a line with an `id` is the answer to a call, and goes to whoever is waiting;
//! - `pty.data` is output, and goes to the terminal's reader;
//! - `pty.exit` is the end, and is the last line the thread reads.
//!
//! One process per pane is deliberate. Multiplexing terminals onto the shared
//! connection would mean the reader thread owning that connection's replies too, and a
//! shell that prints a megabyte would then be in front of every `fs.read` on the host.
//! The cost is one transport process per terminal — the same thing a local pane costs
//! a `fork`, and already paid when the shell itself is started.
//!
//! # Bytes on a JSON wire
//!
//! Output is base64 for the same reason the install script is: a terminal chunk can
//! hold a quote, a newline or half a UTF-8 character, and a JSON string cannot carry
//! those unchanged. The reader below is the decoder, so the daemon's screen parser
//! sees exactly the bytes the program wrote.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdout};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use acpilot_host::{ExecCommand, Size};
use serde_json::{Value, json};

use crate::base64;
use crate::helper::{self, Connections, Transport};

/// A terminal that runs on a host and reports through a helper.
pub(crate) struct RemotePty {
    /// The helper process, owned so that dropping the terminal closes the pipe. The
    /// host's shell is killed by `pty.kill`; a helper that is killed outright takes its
    /// master with it, which hangs the shell up.
    child: Child,
    stdin: std::process::ChildStdin,
    /// Answers to calls, put here by the dispatcher.
    replies: Receiver<Value>,
    next_id: u64,
    size: Size,
    /// What the host said the program exited with, once it has said it. `None` outside
    /// and `None` inside are different: no news yet, versus news that there was no code
    /// (a program a signal ended, or a helper that went away).
    exit: Arc<Mutex<Option<Option<i32>>>>,
    /// The one reader. Two consumers of a terminal's output would each see part of it.
    reader: Option<PtyReader>,
    /// The id the helper knows this terminal by.
    terminal: u32,
    /// For messages: the command line, or the fact that the host chose the shell.
    command: String,
}

impl std::fmt::Debug for RemotePty {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemotePty")
            .field("terminal", &self.terminal)
            .field("size", &self.size)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

impl RemotePty {
    /// Start a terminal on the host `transport` reaches.
    ///
    /// `command` absent means the host's own shell: which one that is belongs to the
    /// host, because the daemon's `SHELL` is about the machine the daemon runs on.
    /// `cwd` is the host's directory, already checked against the workspace root by the
    /// caller — the check is a string one here, since there is no `stat` across the
    /// transport.
    ///
    /// # Errors
    ///
    /// The transport would not start, the helper speaks another protocol, the helper has
    /// no PTY, or the host refused to start the program. The message is the helper's own
    /// words where it has any, so the route can say which side refused.
    pub(crate) fn open(
        connections: &Connections,
        transport: &Transport,
        command: Option<&ExecCommand>,
        cwd: Option<&Path>,
        size: Size,
    ) -> Result<Self, String> {
        let mut session = connections.session(transport)?;
        let handshake = match session.hello() {
            Ok(handshake) => handshake,
            Err(error) => {
                session.end();
                return Err(error);
            }
        };
        if !offers(&handshake, "pty.open") {
            session.end();
            return Err(format!(
                "the helper on {} does not start terminals",
                transport.address
            ));
        }
        let mut params = json!({ "cols": size.cols, "rows": size.rows });
        if let Some(command) = command {
            params["program"] = json!(command.program);
            if !command.args.is_empty() {
                params["args"] = json!(command.args);
            }
        }
        let wanted = command
            .and_then(|command| command.cwd.clone())
            .or_else(|| cwd.map(Path::to_path_buf));
        if let Some(cwd) = wanted {
            params["cwd"] = json!(cwd.display().to_string());
        }
        let opened = match session.request(1, "pty.open", params) {
            Ok(opened) => opened,
            Err(error) => {
                session.end();
                return Err(error);
            }
        };
        let terminal = opened
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| {
                format!("the helper answered pty.open with {opened}, which has no id")
            })?;
        let command_line =
            command.map_or_else(|| "the host's shell".to_owned(), ExecCommand::command_line);

        let (child, stdin, stdout) = session.into_parts();
        let (reply_tx, replies) = mpsc::channel();
        let (bytes_tx, bytes) = mpsc::channel();
        let exit: Arc<Mutex<Option<Option<i32>>>> = Arc::new(Mutex::new(None));
        let heard = Arc::clone(&exit);
        std::thread::Builder::new()
            .name(format!("acpilot-remote-pty-{terminal}"))
            // A line reader and a decoder; the default 2 MiB is for a thread that might
            // recurse, and this one never does.
            .stack_size(128 * 1024)
            .spawn(move || dispatch(stdout, reply_tx, bytes_tx, heard))
            .map_err(|error| format!("{command_line}: {error}"))?;

        Ok(Self {
            child,
            stdin,
            replies,
            next_id: 1,
            size,
            exit,
            reader: Some(PtyReader {
                chunks: bytes,
                rest: Vec::new(),
                offset: 0,
            }),
            terminal,
            command: command_line,
        })
    }

    /// The output side, once.
    pub(crate) fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
        self.reader
            .take()
            .map(|reader| Box::new(reader) as Box<dyn Read + Send>)
    }

    /// Bytes as if typed: one round trip to the host, which is what a remote terminal
    /// costs per keystroke.
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.call(
            "pty.write",
            json!({ "id": self.terminal, "data": base64::encode(bytes) }),
        )
        .map(|_| ())
        .map_err(io::Error::other)
    }

    /// Tell the program its pane changed shape.
    pub(crate) fn resize(&mut self, size: Size) -> Result<(), String> {
        self.call(
            "pty.resize",
            json!({ "id": self.terminal, "cols": size.cols, "rows": size.rows }),
        )?;
        self.size = size;
        Ok(())
    }

    /// The dimensions the pane was last told about.
    pub(crate) fn size(&self) -> Size {
        self.size
    }

    /// What the program exited with, once the host has said so.
    pub(crate) fn exit_code(&self) -> Option<i32> {
        (*self.exit.lock().unwrap_or_else(PoisonError::into_inner)).flatten()
    }

    /// End the program on the host.
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        self.call("pty.kill", json!({ "id": self.terminal }))
            .map(|_| ())
            .map_err(io::Error::other)
    }

    /// One call, waiting for the answer that belongs to it.
    ///
    /// A reply to another id is skipped rather than believed: only one call is ever
    /// outstanding (the terminal's driver holds the lock), so a straggler is an earlier
    /// call's answer arriving late, and attributing it here would hand the caller
    /// somebody else's result.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // A closed pipe is reported by the read below, which says the helper ended
        // rather than naming the symptom twice.
        let _ = writeln!(self.stdin, "{request}").and_then(|()| self.stdin.flush());
        loop {
            let reply = match self.replies.recv_timeout(helper::CALL_TIMEOUT) {
                Ok(reply) => reply,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "{method} on {}: the helper did not answer within {:?}",
                        self.command,
                        helper::CALL_TIMEOUT
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("{method} on {}: the helper ended", self.command));
                }
            };
            if reply.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message");
                return Err(format!(
                    "{method} on {}: {message} (code {code})",
                    self.command
                ));
            }
            return reply.get("result").cloned().ok_or_else(|| {
                format!("the helper answered {method} with neither result nor error")
            });
        }
    }
}

impl Drop for RemotePty {
    fn drop(&mut self) {
        // The helper is this terminal's only owner: killing it closes the pipe, which is
        // what tells the host's shell nobody is listening any more. `pty.kill` is the
        // graceful path and this is the one that also runs when the daemon is going
        // away, so neither a process nor a zombie is left for the next test.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sort the helper's lines until the terminal ends.
fn dispatch(
    stdout: BufReader<ChildStdout>,
    replies: Sender<Value>,
    bytes: Sender<Vec<u8>>,
    exit: Arc<Mutex<Option<Option<i32>>>>,
) {
    for line in stdout.lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // A line that is not JSON is not something to guess at; the call that is
            // waiting will time out and say so, which is the honest failure.
            continue;
        };
        match value.get("method").and_then(Value::as_str) {
            Some("pty.data") => {
                let Some(data) = value["params"]["data"].as_str() else {
                    continue;
                };
                match base64::decode(data) {
                    Ok(chunk) => {
                        if bytes.send(chunk).is_err() {
                            return;
                        }
                    }
                    // Our own helper wrote it; a chunk that does not decode is a decoder
                    // that is wrong, and dropping it is better than printing garbage a
                    // person would take for their shell.
                    Err(_) => continue,
                }
            }
            Some("pty.exit") => {
                let code = value["params"]["code"]
                    .as_i64()
                    .and_then(|code| i32::try_from(code).ok());
                *exit.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
                // The last line this thread reads, and returning drops the last sender:
                // the reader sees EOF, which is how the screen thread learns to stop.
                return;
            }
            // A notification this build does not know is a newer helper's; the pane has
            // no use for it, and guessing would be worse than skipping it.
            Some(_) => continue,
            None => {
                if replies.send(value).is_err() {
                    return;
                }
            }
        }
    }
    // The helper ended without saying how: the reader still has to finish, or a pane
    // would wait forever on a process that is gone.
}

/// The terminal's output, as a [`Read`] the screen pump already knows how to drive.
struct PtyReader {
    chunks: Receiver<Vec<u8>>,
    /// The chunk being handed out, when the caller's buffer was smaller than it.
    rest: Vec<u8>,
    offset: usize,
}

impl Read for PtyReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.offset >= self.rest.len() {
            match self.chunks.recv() {
                Ok(chunk) => {
                    self.rest = chunk;
                    self.offset = 0;
                }
                // Every sender is gone: the helper ended, and so did the screen.
                Err(_) => return Ok(0),
            }
        }
        let read = out.len().min(self.rest.len() - self.offset);
        out[..read].copy_from_slice(&self.rest[self.offset..self.offset + read]);
        self.offset += read;
        Ok(read)
    }
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

/// Nothing in this module is reachable without a host, and a host needs no test seam of
/// its own: the routes' suites stand a shell in for the helper (see
/// `crates/acpilotd/tests/remote_terminals.rs`), which is the same ladder a release
/// daemon walks.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_method_the_helper_named_counts() {
        let handshake = json!({ "methods": ["hello", "fs.read", "exec"] });
        assert!(!offers(&handshake, "pty.open"));
        let handshake = json!({ "methods": ["hello", "pty.open", "pty.write"] });
        assert!(offers(&handshake, "pty.open"));
        assert!(
            !offers(&json!({}), "pty.open"),
            "nothing said, nothing offered"
        );
    }
}
