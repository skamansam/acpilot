//! Talking to `acpilotd-helper` on a host this daemon cannot reach directly.
//!
//! The helper is one process per host, spoken to over stdin/stdout as
//! line-delimited JSON-RPC 2.0 (see `crates/acpilotd-helper`). This module owns the
//! daemon's half of that: put the helper on the host, start the transport, shake hands,
//! say what the far side can do, and then ask it things. [`Connections`] keeps the process
//! that answered, so asking a host the same question twice is one connection rather than
//! two — a helper is a session with a protocol, and a daemon that booted a distro per
//! request would be paying the transport's cost for every question.
//!
//! # Why the helper travels inside the daemon
//!
//! A host that has never seen acpilot has no `acpilotd-helper` on it to start. A release
//! build carries the static binary (see `build.rs`), so the first connection writes it to
//! `~/.local/bin` over the transport that is already there and starts *that* — no `scp`,
//! no package manager, and the version on the host is the version this daemon speaks
//! instead of whatever was installed months ago. A developer build carries nothing and
//! asks the host's `PATH`, which is what every stand-in in this crate's tests is: the
//! difference is packaging, not behaviour.
//!
//! # Why the version is checked
//!
//! The helper is uploaded and reused independently of the daemon, so a new daemon
//! beside an old helper is the expected failure. `hello` names the protocol this
//! build speaks and the helper refuses another by number; a handshake that answered
//! something unexpected is reported rather than parsed optimistically.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use acpilot_proto::host::{HostCapabilities, HostKind};
use serde_json::{Value, json};

use crate::base64::encode_lines;

/// The protocol this daemon speaks.
///
/// Kept in step with `crates/acpilotd-helper/src/main.rs`'s `PROTOCOL`: the two
/// values are the same contract from two sides, and a build that changes one
/// without the other is refused at the handshake instead of failing a call.
pub const PROTOCOL: u64 = 1;

/// The directory an installed helper lives in, relative to the host user's home.
///
/// `~/.local/bin` is the path the design phase's P0-5 measured and the one the WSL and
/// SSH pages tell a person to expect. A relative directory is joined to `$HOME` on the
/// host, so the same constant works on a Linux box, a distro and a Mac.
pub const INSTALL_DIR: &str = ".local/bin";

/// The name the helper is installed under.
pub const INSTALL_NAME: &str = "acpilotd-helper";

/// The helper this build carries, or nothing when it was built without one.
///
/// `build.rs` writes the artifact into `OUT_DIR` and writes an empty file when
/// `ACPILOT_HELPER_BIN` was not set, because `include_bytes!` needs the path to exist at
/// every build. An empty file is not a helper, so it is turned into `None` here rather
/// than offered as a zero-byte program to copy onto a host.
const BUNDLED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/acpilotd-helper"));

/// The static helper this daemon can put on a host, when a build packaged one.
///
/// A release build carries the musl binary, so a host that has never seen acpilot
/// receives it on first connect. A developer build carries nothing and asks the host for
/// `acpilotd-helper` on its `PATH`, which is what every stand-in in this crate's tests
/// is: the difference is packaging, not behaviour.
#[must_use]
pub fn bundled() -> Option<&'static [u8]> {
    (!BUNDLED.is_empty()).then_some(BUNDLED)
}

/// How long one answer is waited for before the helper is called dead.
///
/// A helper is a process on another machine: one that stopped reading would otherwise
/// be a daemon hung on a pipe with nothing to show for it. The budget is the same as
/// the handshake's in `crates/acpilotd/src/hosts.rs`, because a call and a handshake
/// are the same round trip over the same transport.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// A host and the transport command that reaches it.
///
/// The three facts the commands are built from, in one value: a host id is a key, and a
/// key is not a recipe. [`Transport::new`] is what the routes construct from the host
/// they looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// Which of the three transports this host speaks.
    pub kind: HostKind,
    /// The far end's address: `user@machine` for ssh, a distribution for wsl.
    pub address: String,
    /// The `ssh` program, which `ACPILOT_SSH` or a test can move.
    pub ssh: String,
    /// This profile's identity override for the host, when it has one.
    pub identity: Option<acpilot_profile::HostIdentity>,
}

impl Transport {
    /// One host, as the commands need it.
    #[must_use]
    pub fn new(kind: HostKind, address: impl Into<String>, ssh: impl Into<String>) -> Self {
        Self::new_with_identity(kind, address, ssh, None)
    }

    /// One host, with the profile's credentials for it.
    #[must_use]
    pub fn new_with_identity(
        kind: HostKind,
        address: impl Into<String>,
        ssh: impl Into<String>,
        identity: Option<acpilot_profile::HostIdentity>,
    ) -> Self {
        Self {
            kind,
            address: address.into(),
            ssh: ssh.into(),
            identity,
        }
    }
}

/// A helper process whose pipes belong to the caller.
///
/// [`Connection`] reads its helper's stdout on a thread and hands one line to one
/// request, which is the wrong shape for a process that also pushes terminal output.
/// This is the raw end: the same install-and-fall-back ladder, then the pipes, and the
/// caller decides how to read them — [`crate::pty_remote`] reads them on a dispatcher
/// that sorts replies from output.
#[derive(Debug)]
pub struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    /// Start `program` with `args`, without saying anything to it yet.
    fn spawn(program: &str, args: &[String]) -> Result<Self, String> {
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
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Shake hands, and answer with what the helper said it can do.
    ///
    /// The caller checks the list itself: a helper that answers protocol 1 but never
    /// grew a PTY is a host this daemon can read files on and not open a pane on, which
    /// is a message about the host rather than a failed call one at a time.
    ///
    /// # Errors
    ///
    /// The helper answered nothing, answered a line that is not JSON-RPC, answered
    /// another request, or speaks another protocol.
    pub(crate) fn hello(&mut self) -> Result<Value, String> {
        let said = self.request(0, "hello", json!({ "protocol": PROTOCOL }))?;
        let protocol = said
            .get("protocol")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("the helper did not shake hands: {said}"))?;
        if protocol != PROTOCOL {
            return Err(format!(
                "this daemon speaks protocol {PROTOCOL}, the helper {protocol}"
            ));
        }
        Ok(said)
    }

    /// One call, read on this thread.
    ///
    /// Only for the window before anything is streaming — the handshake and the call
    /// that opens a terminal — because after that the pipe belongs to a reader thread and
    /// two readers would split the lines between them.
    pub(crate) fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let _ = writeln!(self.stdin, "{request}").and_then(|()| self.stdin.flush());
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .map_err(|error| format!("{method}: {error}"))?;
            if read == 0 {
                return Err(format!(
                    "{method}: the helper answered nothing and closed its side"
                ));
            }
            let value: Value = serde_json::from_str(&line).map_err(|error| {
                format!("{method}: the helper answered something that is not JSON-RPC: {error}")
            })?;
            if value.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message");
                return Err(format!("{method}: {message} (code {code})"));
            }
            return value
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method}: the helper answered neither result nor error"));
        }
    }

    /// End the process, after a failure that would otherwise leave it running.
    pub(crate) fn end(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The pipes, for a caller that will read them from here on.
    pub(crate) fn into_parts(self) -> (Child, ChildStdin, BufReader<ChildStdout>) {
        (self.child, self.stdin, self.stdout)
    }
}

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
        Self::from_session(Session::spawn(program, args)?, timeout)
    }

    /// A connection over a helper that has already been started.
    ///
    /// Both ladders — "the program the test named" and "install, else `PATH`" — end
    /// here, because a helper that is running is a helper to talk to and the only
    /// difference is which program was run.
    fn from_session(session: Session, timeout: Duration) -> Result<Self, String> {
        let (child, stdin, stdout) = session.into_parts();
        // Reading happens on a thread because a pipe has no read timeout: waiting on
        // it directly would make a helper that stopped reading a daemon that hung,
        // which is the failure a remote path can least afford. The thread ends when the
        // helper does — EOF, or the kill in `Drop` closing the pipe — and the sender
        // being gone is how a caller learns the far side went with it.
        let (sender, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
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

/// The shell script that puts `bytes` at `dir/acpilotd-helper` on a host.
///
/// One `sh -s` (or a `sh -s` behind `ssh`/`wsl.exe`) is the whole transport question:
/// the script arrives on stdin, so nothing has to be quoted for a shell that this
/// daemon cannot see, and the bytes arrive as base64 inside a quoted heredoc, so a
/// binary with a newline in it is not a syntax error.
///
/// The write is to a temporary name and then a rename, so a helper that is running on
/// the host is replaced rather than truncated underneath itself: a half-written
/// executable is a host whose next call fails with `Exec format error`.
fn install_script(dir: &str, bytes: &[u8]) -> String {
    // Only the daemon's own words go in here, but a directory with a quote in it would
    // still end the string early and run the rest as shell.
    let dir = dir.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        concat!(
            "set -e\n",
            "d=\"{dir}\"\n",
            "mkdir -p \"$d\"\n",
            "base64 -d > \"$d/.{name}.new\" <<'ACPILOT-HELPER'\n",
            "{encoded}\n",
            "ACPILOT-HELPER\n",
            "chmod 755 \"$d/.{name}.new\"\n",
            "mv \"$d/.{name}.new\" \"$d/{name}\"\n",
        ),
        dir = dir,
        name = INSTALL_NAME,
        encoded = encode_lines(bytes),
    )
}

/// Run `program args` — a shell that reads its script from stdin — and wait for it.
///
/// The wait is bounded because the transport is a process on another machine: an `ssh`
/// whose host stopped answering would otherwise be a daemon parked forever in a
/// blocking thread. A failure carries the far side's own stderr, which is the only
/// thing that says *why* (`mkdir: Permission denied` versus `sh: not found`).
fn install_within(
    program: &str,
    args: &[String],
    dir: &str,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A write that fails is reported by the exit status below, which names the
        // shell's own reason; the pipe's `EPIPE` is the symptom of the same thing.
        let _ = stdin
            .write_all(install_script(dir, bytes).as_bytes())
            .and_then(|()| stdin.flush());
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut said = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                return Err(format!(
                    "putting {INSTALL_NAME} in {dir} on the host failed ({status}): {}",
                    said.trim()
                ));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "putting {INSTALL_NAME} on the host did not finish within {timeout:?}"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("{program}: {error}")),
        }
    }
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
    /// Where an installed helper goes, when a test or a build moves it off `$HOME`.
    dir: Mutex<Option<String>>,
    /// The bytes to install, when a test supplies them instead of the build.
    bundle: Mutex<Option<Vec<u8>>>,
}

impl Connections {
    /// No connection has been opened yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Say where a host should put the helper, without touching this process's
    /// environment.
    ///
    /// `ACPILOT_HELPER_DIR` is the daemon's own answer and this is a test's: a suite that
    /// set the variable would be choosing the install directory for every test beside
    /// it, and one that installed into the developer's real `~/.local/bin` would pass
    /// by overwriting the thing it claims to have created.
    pub fn prefer_install_dir(&self, dir: Option<String>) {
        *self.dir.lock().unwrap_or_else(PoisonError::into_inner) = dir;
    }

    /// Say which helper to put on a host, overriding the one this build carries.
    ///
    /// The bundled bytes are compiled in, so a test cannot produce them; this is the
    /// seam that lets the install path be exercised by a build that carries nothing.
    /// `None` means "whatever the build carries", which is also the default.
    pub fn prefer_bundle(&self, bytes: Option<Vec<u8>>) {
        *self.bundle.lock().unwrap_or_else(PoisonError::into_inner) = bytes;
    }

    /// The directory an installed helper goes in, as a shell spells it.
    fn install_dir(&self) -> String {
        self.dir
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .or_else(|| std::env::var("ACPILOT_HELPER_DIR").ok())
            .unwrap_or_else(|| INSTALL_DIR.to_owned())
    }

    /// The helper bytes to install: a test's, else this build's.
    fn bundle(&self) -> Option<Vec<u8>> {
        self.bundle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .or_else(|| bundled().map(<[u8]>::to_vec))
    }

    /// Test host `id` through `transport`, reusing the helper it already answered on.
    ///
    /// # Errors
    ///
    /// The install, the transport or the handshake failed on every path this daemon
    /// knows, and the message names each in turn.
    pub fn probe(&self, id: &str, transport: &Transport) -> Result<HostCapabilities, String> {
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
        let connection = self.dial(transport)?;
        let capabilities = connection.capabilities();
        live.insert(id.to_owned(), connection);
        Ok(capabilities)
    }

    /// Ask host `id` `method`, dialling first when there is no connection yet.
    ///
    /// # Errors
    ///
    /// The transport would not start, the handshake failed, or the call itself did:
    /// a missing file and a dead helper are both failures here, and the message is
    /// the helper's own words so the route can say which side refused.
    pub fn request(
        &self,
        id: &str,
        transport: &Transport,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if !live.contains_key(id) {
            let connection = self.dial(transport)?;
            live.insert(id.to_owned(), connection);
        }
        let Some(connection) = live.get_mut(id) else {
            return Err("the connection went while it was dialling".to_owned());
        };
        let answer = connection.request(method, params);
        if answer.is_err() {
            // A call that failed may have left the pipe mid-conversation — a refused
            // path returns a well-formed error, but a timeout or a broken line does
            // not — and the next request would read the rest of this one instead of
            // its own answer. Dropping it makes the next call dial again.
            live.remove(id);
        }
        answer
    }

    /// Test host `id` through a program that is already on the host, installing nothing.
    ///
    /// The seam the transport tests use: they stand `sh` in for a helper, which is not a
    /// program this daemon can upload.
    ///
    /// # Errors
    ///
    /// As [`Connection::dial`].
    pub fn probe_with(
        &self,
        id: &str,
        program: &str,
        args: &[String],
    ) -> Result<HostCapabilities, String> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(connection) = live.get_mut(id) {
            match connection.hello() {
                Ok(()) => return Ok(connection.capabilities()),
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

    /// Ask host `id` `method` on a program that is already there.
    ///
    /// # Errors
    ///
    /// As [`Connection::dial`] and [`Connection::request`].
    pub fn request_with(
        &self,
        id: &str,
        program: &str,
        args: &[String],
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if !live.contains_key(id) {
            let connection = Connection::dial(program, args)?;
            live.insert(id.to_owned(), connection);
        }
        let Some(connection) = live.get_mut(id) else {
            return Err("the connection went while it was dialling".to_owned());
        };
        let answer = connection.request(method, params);
        if answer.is_err() {
            live.remove(id);
        }
        answer
    }

    /// Start the helper on a host: install it when this build carries one, else use
    /// the one on the host's `PATH`.
    ///
    /// The install is attempted once per connection, not once per call: `request` and
    /// `probe` only dial when nothing is live, so a helper that answered is not
    /// uploaded over itself. A host that refuses the write — a read-only home, a
    /// sandbox with no `sh` — still gets the `PATH` attempt, because a machine that
    /// was prepared by hand is a machine that works.
    /// A helper whose pipes the caller reads, started by the same ladder.
    pub(crate) fn session(&self, transport: &Transport) -> Result<Session, String> {
        self.start(transport, Ok)
    }

    /// The same helper, with the reply queue a request/response conversation needs.
    fn dial(&self, transport: &Transport) -> Result<Connection, String> {
        self.start(transport, |session| {
            Connection::from_session(session, CALL_TIMEOUT)
        })
    }

    /// The install-and-fall-back ladder, and what to build from the pipes.
    ///
    /// The handshake happens inside `connect` so that a helper which starts and then
    /// fails to answer is still part of the *ladder's* answer: a host that refuses the
    /// install and then has an old helper on `PATH` has two problems, and reporting only
    /// the second would hide the one somebody has to fix.
    fn start<T>(
        &self,
        transport: &Transport,
        connect: impl Fn(Session) -> Result<T, String>,
    ) -> Result<T, String> {
        let dir = self.install_dir();
        let mut problems = Vec::new();
        if let Some(bytes) = self.bundle() {
            let (program, args) = crate::hosts::remote_command_with(
                transport.kind,
                &transport.address,
                &transport.ssh,
                transport.identity.as_ref(),
                crate::hosts::Step::Install,
                &dir,
            );
            match install_within(&program, &args, &dir, &bytes, CALL_TIMEOUT) {
                Ok(()) => {
                    let (program, args) = crate::hosts::remote_command_with(
                        transport.kind,
                        &transport.address,
                        &transport.ssh,
                        transport.identity.as_ref(),
                        crate::hosts::Step::Installed,
                        &dir,
                    );
                    match Session::spawn(&program, &args).and_then(&connect) {
                        Ok(started) => return Ok(started),
                        Err(error) => problems.push(format!("the installed helper: {error}")),
                    }
                }
                Err(error) => problems.push(error),
            }
        }
        let (program, args) = crate::hosts::remote_command_with(
            transport.kind,
            &transport.address,
            &transport.ssh,
            transport.identity.as_ref(),
            crate::hosts::Step::Path,
            &dir,
        );
        Session::spawn(&program, &args)
            .and_then(&connect)
            .map_err(|error| {
                if problems.is_empty() {
                    return error;
                }
                problems.push(format!("{INSTALL_NAME} on the host's PATH: {error}"));
                problems.join("; ")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_script_writes_exactly_the_bytes_it_was_given() {
        // The install script is POSIX `sh` on purpose — a host this reaches is a unix
        // host — and a build without one carries no helper to install at all. There is
        // nothing to assert where there is no shell to assert it with, and this is a
        // runtime check rather than a `cfg` because the workspace's boundary test says a
        // `cfg` naming an operating system belongs in `src/platform/`.
        if std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .is_err()
        {
            return;
        }
        // Every byte value, so a newline, a NUL and the heredoc's own delimiter are all
        // somewhere in it: the install is a text channel for a binary, and that is the
        // thing that breaks quietly.
        let bytes: Vec<u8> = (0..=255u8).cycle().take(4_000).collect();
        let directory = tempfile::tempdir().expect("temp dir");
        let dir = directory.path().join("bin");
        install_within(
            "sh",
            &["-s".to_owned()],
            &dir.display().to_string(),
            &bytes,
            Duration::from_secs(10),
        )
        .expect("the script runs");
        let written = std::fs::read(dir.join(INSTALL_NAME)).expect("the installed helper");
        // Compared rather than printed: a failing assert_eq on four thousand bytes is a
        // wall of decimals that says nothing about where the two differ.
        let same = written.len() == bytes.len()
            && written
                .iter()
                .zip(&bytes)
                .all(|(written, wanted)| written == wanted);
        assert!(
            same,
            "the install is not byte for byte: wrote {} bytes, expected {}",
            written.len(),
            bytes.len()
        );
    }
}
