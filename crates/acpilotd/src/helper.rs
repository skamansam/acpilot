//! Talking to `acpilotd-helper` on a host this daemon cannot reach directly.
//!
//! The helper is one process per host, spoken to over stdin/stdout as
//! line-delimited JSON-RPC 2.0 (see `crates/acpilotd-helper`). This module owns the
//! daemon's half of that: start the transport, shake hands, and report what the far
//! side can do. [`Connections`] keeps the process that answered, so asking a host the
//! same question twice is one connection rather than two — a helper is a session with
//! a protocol, and a daemon that booted a distro per request would be paying the
//! transport's cost for every question.
//!
//! # Why the version is checked
//!
//! The helper is uploaded and reused independently of the daemon, so a new daemon
//! beside an old helper is the expected failure. `hello` names the protocol this
//! build speaks and the helper refuses another by number; a handshake that answered
//! something unexpected is reported rather than parsed optimistically.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, PoisonError};

use acpilot_proto::host::HostCapabilities;
use serde_json::{Value, json};

/// The protocol this daemon speaks.
///
/// Kept in step with `crates/acpilotd-helper/src/main.rs`'s `PROTOCOL`: the two
/// values are the same contract from two sides, and a build that changes one
/// without the other is refused at the handshake instead of failing a call.
pub const PROTOCOL: u64 = 1;

/// One running helper, and the pipes it answers on.
///
/// Dropped when the daemon forgets the host or a call fails: a helper that stopped
/// reading is not a connection worth keeping, and a process left behind would be a
/// leak per failed test.
#[derive(Debug)]
pub struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    capabilities: HostCapabilities,
    next_id: u64,
}

impl Connection {
    /// Start `program` with `args` and shake hands with it.
    ///
    /// # Errors
    ///
    /// The program could not start, wrote nothing, answered something that is not a
    /// handshake, or speaks another protocol. The message names what happened; the
    /// caller turns it into a `502` for the host.
    pub fn dial(program: &str, args: &[String]) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the helper has no stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the helper has no stdout".to_owned())?;
        let mut connection = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            // Filled in by the handshake: the helper names its methods and this is
            // what it says, not what this build expects of it. Nothing is true until
            // it has answered, so a half-finished connection offers nothing.
            capabilities: HostCapabilities {
                filesystem: false,
                pty: false,
                spawn: false,
                lsp: false,
            },
            next_id: 0,
        };
        // A failed handshake drops `connection`, which kills the child: a transport
        // that started and then said nothing is not left running for nobody.
        connection.hello()?;
        Ok(connection)
    }

    /// What this helper can do, as the handshake defined it.
    #[must_use]
    pub fn capabilities(&self) -> HostCapabilities {
        self.capabilities
    }

    /// Ask the helper what it is, on the connection it already answered on.
    ///
    /// The reply's `id` is not matched: the conversation is one request and one
    /// answer at a time, and the mutex that owns this connection is the thing that
    /// keeps it that way.
    fn hello(&mut self) -> Result<(), String> {
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "hello",
            "params": { "protocol": PROTOCOL },
        });
        // A program that exited before reading is not a helper, and it answers
        // nothing — which the read below reports as itself. Treating the write's
        // `EPIPE` as the failure instead would name the symptom (a broken pipe) rather
        // than the fact (no handshake), and would report it twice for one cause.
        let _ = writeln!(self.stdin, "{request}").and_then(|()| self.stdin.flush());
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|error| format!("the helper would not answer: {error}"))?;
        if line.trim().is_empty() {
            return Err("the helper answered nothing".to_owned());
        }
        let response: Value = serde_json::from_str(&line).map_err(|error| {
            format!("the helper answered something that is not JSON-RPC: {error}")
        })?;
        let protocol = response
            .get("result")
            .and_then(|result| result.get("protocol"))
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("the helper did not shake hands: {}", line.trim()))?;
        if protocol != PROTOCOL {
            return Err(format!(
                "this daemon speaks protocol {PROTOCOL}, the helper {protocol}"
            ));
        }
        self.capabilities = capabilities(response.get("result").unwrap_or(&Value::Null));
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Killing and reaping on drop is what makes "forget this host" and "this
        // transport failed" the same operation: neither a stale process nor a
        // zombie is left for the next test to trip over.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the far side said it can do, read out of the handshake's `result`.
///
/// A helper that lists its methods is believed, because the list is its own: a build
/// that grows a PTY offers `pty.*` and the daemon stops guessing, and one without
/// `exec` is not offered as a host to spawn an agent on. The navigator's set is the
/// fallback for a helper from before the list existed — protocol 1 always meant files
/// and programs — and an explicit empty list is not that fallback: a helper that says
/// it can do nothing is not one to hand a workspace to.
fn capabilities(handshake: &Value) -> HostCapabilities {
    let Some(methods) = handshake.get("methods").and_then(Value::as_array) else {
        return HostCapabilities {
            filesystem: true,
            pty: false,
            spawn: true,
            lsp: false,
        };
    };
    let offers = |prefix: &str| {
        methods
            .iter()
            .filter_map(Value::as_str)
            .any(|method| method.starts_with(prefix))
    };
    HostCapabilities {
        filesystem: offers("fs."),
        pty: offers("pty."),
        spawn: offers("exec"),
        lsp: offers("lsp."),
    }
}

/// Ask a helper that is started for this call only, and end it afterwards.
///
/// # Errors
///
/// As [`Connection::dial`].
pub fn probe(program: &str, args: &[String]) -> Result<HostCapabilities, String> {
    Ok(Connection::dial(program, args)?.capabilities())
}

/// The helpers this daemon has running, one per host.
///
/// A failed call drops the connection, so the next test dials again rather than
/// reporting the same dead pipe for the rest of the process. The lock is held across
/// the handshake because it *is* the connection: two requests on one helper would
/// interleave two conversations on one pair of pipes.
#[derive(Debug, Default)]
pub struct Connections {
    live: Mutex<HashMap<String, Connection>>,
}

impl Connections {
    /// No connection has been opened yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Test host `id` through `program`, reusing the helper it already answered on.
    ///
    /// # Errors
    ///
    /// [`Connection::dial`] failed, or the kept helper stopped answering and the
    /// fresh one failed too.
    pub fn probe(
        &self,
        id: &str,
        program: &str,
        args: &[String],
    ) -> Result<HostCapabilities, String> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(connection) = live.get_mut(id) {
            match connection.hello() {
                Ok(()) => return Ok(connection.capabilities()),
                // The helper died, or the transport dropped it: the next dial is the
                // answer. Keeping a broken pipe would fail every later test of a host
                // that has come back.
                Err(_) => {
                    live.remove(id);
                }
            }
        }
        let connection = Connection::dial(program, args)?;
        let capabilities = connection.capabilities();
        live.insert(id.to_owned(), connection);
        Ok(capabilities)
    }
}
