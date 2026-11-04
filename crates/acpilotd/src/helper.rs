//! Talking to `acpilotd-helper` on a host this daemon cannot reach directly.
//!
//! The helper is one process per host, spoken to over stdin/stdout as
//! line-delimited JSON-RPC 2.0 (see `crates/acpilotd-helper`). This module owns the
//! daemon's half of that: start the transport, shake hands, say what the far side can
//! do, and then ask it things. [`Connections`] keeps the process that answered, so asking a host the
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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use acpilot_proto::host::HostCapabilities;
use serde_json::{Value, json};

/// The protocol this daemon speaks.
///
/// Kept in step with `crates/acpilotd-helper/src/main.rs`'s `PROTOCOL`: the two
/// values are the same contract from two sides, and a build that changes one
/// without the other is refused at the handshake instead of failing a call.
pub const PROTOCOL: u64 = 1;

/// How long one answer is waited for before the helper is called dead.
///
/// A helper is a process on another machine: one that stopped reading would otherwise
/// be a daemon hung on a pipe with nothing to show for it. The budget is the same as
/// the handshake's in `crates/acpilotd/src/hosts.rs`, because a call and a handshake
/// are the same round trip over the same transport.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// One running helper, and the pipes it answers on.
///
/// Dropped when the daemon forgets the host or a call fails: a helper that stopped
/// reading is not a connection worth keeping, and a process left behind would be a
/// leak per failed test.
#[derive(Debug)]
pub struct Connection {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<String>,
    timeout: Duration,
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
        Self::dial_within(program, args, CALL_TIMEOUT)
    }

    /// As [`Connection::dial`], waiting `timeout` for each answer.
    ///
    /// # Errors
    ///
    /// As [`Connection::dial`], plus a helper that did not answer inside `timeout`.
    pub fn dial_within(program: &str, args: &[String], timeout: Duration) -> Result<Self, String> {
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
        // Reading happens on a thread because a pipe has no read timeout: waiting on
        // it directly would make a helper that stopped reading a daemon that hung,
        // which is the failure a remote path can least afford. The thread ends when the
        // helper does — EOF, or the kill in `Drop` closing the pipe — and the sender
        // being gone is how a caller learns the far side went with it.
        let (sender, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut connection = Self {
            child,
            stdin,
            replies,
            timeout,
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

    /// Ask the helper `method` with `params` and hand back its `result`.
    ///
    /// # Errors
    ///
    /// The helper answered nothing, answered a line that is not JSON-RPC, answered a
    /// different request, refused with an error, or stayed silent past the timeout.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id + 1;
        self.next_id = id;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        // A program that exited before reading is not a helper, and it answers
        // nothing — which the read below reports as itself. Treating the write's
        // `EPIPE` as the failure instead would name the symptom (a broken pipe) rather
        // than the fact, and would report it twice for one cause.
        let _ = writeln!(self.stdin, "{request}").and_then(|()| self.stdin.flush());
        let line = self.read()?;
        let response: Value = serde_json::from_str(&line).map_err(|error| {
            format!("the helper answered something that is not JSON-RPC: {error}")
        })?;
        // The id is checked rather than assumed: a reply to another request taken as
        // this one's would hand the caller somebody else's file, and the interleaving
        // that produced it is the thing to report, not to paper over.
        match response.get("id") {
            Some(replied) if *replied == json!(id) => {}
            Some(other) => return Err(format!("the helper answered request {other}, not {id}")),
            None => {
                return Err(format!(
                    "the helper answered without saying which request: {}",
                    line.trim()
                ));
            }
        }
        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(format!("{method}: {message} (code {code})"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("the helper answered {method} with neither result nor error"))
    }

    /// Ask the helper what it is, on the connection it already answered on.
    fn hello(&mut self) -> Result<(), String> {
        let result = self.request("hello", json!({ "protocol": PROTOCOL }))?;
        let protocol = result
            .get("protocol")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("the helper did not shake hands: {result}"))?;
        if protocol != PROTOCOL {
            return Err(format!(
                "this daemon speaks protocol {PROTOCOL}, the helper {protocol}"
            ));
        }
        self.capabilities = capabilities(&result);
        Ok(())
    }

    /// One line from the helper, or what happened instead of one.
    fn read(&mut self) -> Result<String, String> {
        match self.replies.recv_timeout(self.timeout) {
            Ok(line) if line.trim().is_empty() => Err("the helper answered nothing".to_owned()),
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "the helper did not answer within {:?}",
                self.timeout
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("the helper answered nothing and closed its side".to_owned())
            }
        }
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
